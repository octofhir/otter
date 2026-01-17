//! Worker extension module using the new architecture.
//!
//! This module provides the Worker extension with Web Worker API.
//!
//! ## Architecture
//!
//! - `worker.rs` - Rust Worker implementation
//! - `worker_ext.rs` - Extension creation with ops
//! - `worker.js` - JavaScript Worker class wrapper
//!
//! Note: This module uses shared state (WorkerManager) which doesn't fit the #[dive]
//! pattern, so we use traditional op_sync with closures.

use otter_runtime::extension::{op_sync, OpDecl};
use otter_runtime::Extension;
use serde_json::json;
use std::sync::Arc;

use crate::worker;

/// Create the worker extension.
///
/// This extension provides Web Worker API for running JavaScript in background threads.
pub fn extension() -> Extension {
    // Shared worker manager
    let manager = Arc::new(worker::WorkerManager::new());

    let mut ops: Vec<OpDecl> = Vec::new();

    // workerCreate(script) -> worker_id
    let mgr_create = manager.clone();
    ops.push(op_sync("workerCreate", move |_ctx, args| {
        let script = args
            .first()
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let id = mgr_create
            .create(script)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(id))
    }));

    // workerPostMessage(id, data) -> null
    let mgr_post = manager.clone();
    ops.push(op_sync("workerPostMessage", move |_ctx, args| {
        let id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("workerPostMessage requires id")
        })? as u32;

        let data = args.get(1).cloned().unwrap_or(json!(null));

        mgr_post
            .post_message(id, data)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(null))
    }));

    // workerTerminate(id) -> null
    let mgr_terminate = manager.clone();
    ops.push(op_sync("workerTerminate", move |_ctx, args| {
        let id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("workerTerminate requires id")
        })? as u32;

        mgr_terminate
            .terminate(id)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(null))
    }));

    // workerPollEvents() -> array of events
    let mgr_poll = manager.clone();
    ops.push(op_sync("workerPollEvents", move |_ctx, _args| {
        let events = mgr_poll.poll_events();
        let json_events: Vec<serde_json::Value> = events
            .into_iter()
            .map(|(id, event)| match event {
                worker::WorkerEvent::Message(data) => json!({
                    "id": id,
                    "type": "message",
                    "data": data
                }),
                worker::WorkerEvent::Error(msg) => json!({
                    "id": id,
                    "type": "error",
                    "message": msg
                }),
                worker::WorkerEvent::Exit => json!({
                    "id": id,
                    "type": "exit"
                }),
                worker::WorkerEvent::Terminated => json!({
                    "id": id,
                    "type": "terminated"
                }),
            })
            .collect();

        Ok(json!(json_events))
    }));

    Extension::new("Worker")
        .with_ops(ops)
        .with_js(include_str!("worker.js"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension();
        assert_eq!(ext.name(), "Worker");
        assert!(ext.js_code().is_some());
    }
}
