//! Worker threads extension module.
//!
//! This module provides the worker_threads extension for Node.js-compatible worker threads.
//!
//! ## Architecture
//!
//! - `worker_threads.rs` - Rust worker thread implementation
//! - `worker_threads_ext.rs` - Extension creation with ops
//! - `worker_threads.js` - JavaScript wrapper
//!
//! Note: This module uses shared state (WorkerThreadManager) which doesn't fit the #[dive]
//! pattern, so we use traditional op_sync with closures.

use otter_runtime::Extension;
use otter_runtime::extension::{OpDecl, op_sync};
use serde_json::json;
use std::sync::Arc;

use crate::worker_threads::{
    ActiveWorkerCount, ResourceLimits, WorkerThreadManager, WorkerThreadOptions,
};

/// Create the worker_threads extension.
///
/// This extension provides Node.js-compatible worker threads API.
///
/// Returns a tuple of (Extension, ActiveWorkerCount) so the runtime can track
/// when all workers have stopped.
pub fn extension() -> (Extension, ActiveWorkerCount) {
    let manager = Arc::new(WorkerThreadManager::new());
    let active_count = manager.active_count_ref();

    let mut ops: Vec<OpDecl> = Vec::new();

    // ========== Worker Thread Operations ==========

    // __workerThreadsIsMainThread() -> bool
    let mgr_main = manager.clone();
    ops.push(op_sync(
        "__workerThreadsIsMainThread",
        move |_ctx, _args| Ok(json!(mgr_main.is_main_thread())),
    ));

    // __workerThreadsThreadId() -> number
    let mgr_tid = manager.clone();
    ops.push(op_sync("__workerThreadsThreadId", move |_ctx, _args| {
        Ok(json!(mgr_tid.thread_id()))
    }));

    // __workerThreadsCreate(options) -> worker_id
    let mgr_create = manager.clone();
    ops.push(op_sync("__workerThreadsCreate", move |_ctx, args| {
        let options = parse_worker_options(args.first());

        let id = mgr_create
            .create(options)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(id))
    }));

    // __workerThreadsPostMessage(id, data, transferList?) -> null
    let mgr_post = manager.clone();
    ops.push(op_sync("__workerThreadsPostMessage", move |_ctx, args| {
        let id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("workerThreadsPostMessage requires id")
        })? as u32;

        let data = args.get(1).cloned().unwrap_or(json!(null));
        let transfer_list = args
            .get(2)
            .and_then(|v| v.as_array())
            .map(|arr| arr.clone());

        mgr_post
            .post_message(id, data, transfer_list)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(null))
    }));

    // __workerThreadsTerminate(id) -> null
    let mgr_term = manager.clone();
    ops.push(op_sync("__workerThreadsTerminate", move |_ctx, args| {
        let id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("workerThreadsTerminate requires id")
        })? as u32;

        mgr_term
            .terminate(id)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(null))
    }));

    // __workerThreadsRef(id) -> null
    let mgr_ref = manager.clone();
    ops.push(op_sync("__workerThreadsRef", move |_ctx, args| {
        let id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("workerThreadsRef requires id")
        })? as u32;

        mgr_ref.ref_worker(id);
        Ok(json!(null))
    }));

    // __workerThreadsUnref(id) -> null
    let mgr_unref = manager.clone();
    ops.push(op_sync("__workerThreadsUnref", move |_ctx, args| {
        let id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("workerThreadsUnref requires id")
        })? as u32;

        mgr_unref.unref_worker(id);
        Ok(json!(null))
    }));

    // __workerThreadsGetResourceLimits(id?) -> object
    let mgr_limits = manager.clone();
    ops.push(op_sync(
        "__workerThreadsGetResourceLimits",
        move |_ctx, args| {
            let limits = if let Some(id) = args.first().and_then(|v| v.as_u64()) {
                mgr_limits.get_resource_limits(id as u32)
            } else {
                None
            };

            Ok(match limits {
                Some(l) => json!({
                    "maxYoungGenerationSizeMb": l.max_young_generation_size_mb,
                    "maxOldGenerationSizeMb": l.max_old_generation_size_mb,
                    "codeRangeSizeMb": l.code_range_size_mb,
                    "stackSizeMb": l.stack_size_mb,
                }),
                None => json!({}),
            })
        },
    ));

    // __workerThreadsPollEvents() -> events[]
    let mgr_poll = manager.clone();
    ops.push(op_sync("__workerThreadsPollEvents", move |_ctx, _args| {
        let events = mgr_poll.poll_events();
        let json_events: Vec<serde_json::Value> = events
            .into_iter()
            .map(|event| match event {
                crate::worker_threads::WorkerThreadEvent::Online { worker_id } => {
                    json!({"type": "online", "workerId": worker_id})
                }
                crate::worker_threads::WorkerThreadEvent::Message { worker_id, data } => {
                    json!({"type": "message", "workerId": worker_id, "data": data})
                }
                crate::worker_threads::WorkerThreadEvent::MessageError { worker_id, error } => {
                    json!({"type": "messageerror", "workerId": worker_id, "error": error})
                }
                crate::worker_threads::WorkerThreadEvent::Error { worker_id, error } => {
                    json!({"type": "error", "workerId": worker_id, "error": error})
                }
                crate::worker_threads::WorkerThreadEvent::Exit { worker_id, code } => {
                    json!({"type": "exit", "workerId": worker_id, "code": code})
                }
                crate::worker_threads::WorkerThreadEvent::PortMessage { port_id, data } => {
                    json!({"type": "portMessage", "portId": port_id, "data": data})
                }
                crate::worker_threads::WorkerThreadEvent::PortMessageError { port_id, error } => {
                    json!({"type": "portMessageError", "portId": port_id, "error": error})
                }
                crate::worker_threads::WorkerThreadEvent::PortClose { port_id } => {
                    json!({"type": "portClose", "portId": port_id})
                }
                crate::worker_threads::WorkerThreadEvent::BroadcastMessage {
                    channel_id,
                    name,
                    data,
                } => {
                    json!({"type": "broadcastMessage", "channelId": channel_id, "name": name, "data": data})
                }
                crate::worker_threads::WorkerThreadEvent::BroadcastMessageError {
                    channel_id,
                    name,
                    error,
                } => {
                    json!({"type": "broadcastMessageError", "channelId": channel_id, "name": name, "error": error})
                }
            })
            .collect();

        Ok(json!(json_events))
    }));

    // ========== MessageChannel / MessagePort Operations ==========

    // __messageChannelCreate() -> {port1Id, port2Id}
    let mgr_mc = manager.clone();
    ops.push(op_sync("__messageChannelCreate", move |_ctx, _args| {
        let (port1_id, port2_id) = mgr_mc.create_message_channel();
        Ok(json!({"port1Id": port1_id, "port2Id": port2_id}))
    }));

    // __messagePortPostMessage(portId, data, transferList?) -> null
    let mgr_mp_post = manager.clone();
    ops.push(op_sync("__messagePortPostMessage", move |_ctx, args| {
        let port_id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("messagePortPostMessage requires portId")
        })?;

        let data = args.get(1).cloned().unwrap_or(json!(null));
        let transfer_list = args
            .get(2)
            .and_then(|v| v.as_array())
            .map(|arr| arr.clone());

        mgr_mp_post
            .port_post_message(port_id, data, transfer_list)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(null))
    }));

    // __messagePortStart(portId) -> null
    let mgr_mp_start = manager.clone();
    ops.push(op_sync("__messagePortStart", move |_ctx, args| {
        let port_id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("messagePortStart requires portId")
        })?;

        mgr_mp_start
            .port_start(port_id)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(null))
    }));

    // __messagePortClose(portId) -> null
    let mgr_mp_close = manager.clone();
    ops.push(op_sync("__messagePortClose", move |_ctx, args| {
        let port_id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("messagePortClose requires portId")
        })?;

        mgr_mp_close
            .port_close(port_id)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(null))
    }));

    // __messagePortRef(portId) -> null
    let mgr_mp_ref = manager.clone();
    ops.push(op_sync("__messagePortRef", move |_ctx, args| {
        let port_id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("messagePortRef requires portId")
        })?;

        mgr_mp_ref.port_ref(port_id);
        Ok(json!(null))
    }));

    // __messagePortUnref(portId) -> null
    let mgr_mp_unref = manager.clone();
    ops.push(op_sync("__messagePortUnref", move |_ctx, args| {
        let port_id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("messagePortUnref requires portId")
        })?;

        mgr_mp_unref.port_unref(port_id);
        Ok(json!(null))
    }));

    // __messagePortHasRef(portId) -> bool
    let mgr_mp_hasref = manager.clone();
    ops.push(op_sync("__messagePortHasRef", move |_ctx, args| {
        let port_id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("messagePortHasRef requires portId")
        })?;

        Ok(json!(mgr_mp_hasref.port_has_ref(port_id)))
    }));

    // __receiveMessageOnPort(portId) -> message | undefined
    let mgr_recv = manager.clone();
    ops.push(op_sync("__receiveMessageOnPort", move |_ctx, args| {
        let port_id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("receiveMessageOnPort requires portId")
        })?;

        match mgr_recv.receive_message_on_port(port_id) {
            Some(data) => Ok(json!({"message": data})),
            None => Ok(json!(null)),
        }
    }));

    // ========== BroadcastChannel Operations ==========

    // __broadcastChannelCreate(name) -> channelId
    let mgr_bc_create = manager.clone();
    ops.push(op_sync("__broadcastChannelCreate", move |_ctx, args| {
        let name = args
            .first()
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                otter_runtime::error::JscError::internal("broadcastChannelCreate requires name")
            })?
            .to_string();

        let id = mgr_bc_create.create_broadcast_channel(name);
        Ok(json!(id))
    }));

    // __broadcastChannelPostMessage(channelId, data) -> null
    let mgr_bc_post = manager.clone();
    ops.push(op_sync(
        "__broadcastChannelPostMessage",
        move |_ctx, args| {
            let channel_id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal(
                    "broadcastChannelPostMessage requires channelId",
                )
            })?;

            let data = args.get(1).cloned().unwrap_or(json!(null));

            mgr_bc_post
                .broadcast_post_message(channel_id, data)
                .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

            Ok(json!(null))
        },
    ));

    // __broadcastChannelClose(channelId) -> null
    let mgr_bc_close = manager.clone();
    ops.push(op_sync("__broadcastChannelClose", move |_ctx, args| {
        let channel_id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("broadcastChannelClose requires channelId")
        })?;

        mgr_bc_close
            .broadcast_close(channel_id)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(null))
    }));

    // __broadcastChannelRef(channelId) -> null
    let mgr_bc_ref = manager.clone();
    ops.push(op_sync("__broadcastChannelRef", move |_ctx, args| {
        let channel_id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("broadcastChannelRef requires channelId")
        })?;

        mgr_bc_ref.broadcast_ref(channel_id);
        Ok(json!(null))
    }));

    // __broadcastChannelUnref(channelId) -> null
    let mgr_bc_unref = manager.clone();
    ops.push(op_sync("__broadcastChannelUnref", move |_ctx, args| {
        let channel_id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("broadcastChannelUnref requires channelId")
        })?;

        mgr_bc_unref.broadcast_unref(channel_id);
        Ok(json!(null))
    }));

    // ========== Environment Data Operations ==========

    // __workerThreadsGetEnvData(key) -> value | undefined
    let mgr_get_env = manager.clone();
    ops.push(op_sync("__workerThreadsGetEnvData", move |_ctx, args| {
        let key = args.first().and_then(|v| v.as_str()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("getEnvironmentData requires key")
        })?;

        match mgr_get_env.get_environment_data(key) {
            Some(data) => Ok(data),
            None => Ok(json!(null)),
        }
    }));

    // __workerThreadsSetEnvData(key, value) -> null
    let mgr_set_env = manager.clone();
    ops.push(op_sync("__workerThreadsSetEnvData", move |_ctx, args| {
        let key = args
            .first()
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                otter_runtime::error::JscError::internal("setEnvironmentData requires key")
            })?
            .to_string();

        let value = args.get(1).cloned().unwrap_or(json!(null));
        mgr_set_env.set_environment_data(key, value);

        Ok(json!(null))
    }));

    // ========== Untransferable Operations ==========

    // __markAsUntransferable() -> id
    let mgr_mark = manager.clone();
    ops.push(op_sync("__markAsUntransferable", move |_ctx, _args| {
        let id = mgr_mark.mark_as_untransferable();
        Ok(json!(id))
    }));

    // __isMarkedAsUntransferable(id) -> bool
    let mgr_is_marked = manager.clone();
    ops.push(op_sync("__isMarkedAsUntransferable", move |_ctx, args| {
        let id = args.first().and_then(|v| v.as_u64()).unwrap_or(0);
        Ok(json!(mgr_is_marked.is_marked_as_untransferable(id)))
    }));

    let ext = Extension::new("worker_threads")
        .with_ops(ops)
        .with_js(include_str!("worker_threads.js"));

    (ext, active_count)
}

/// Parse worker thread options from JSON value.
fn parse_worker_options(value: Option<&serde_json::Value>) -> WorkerThreadOptions {
    let Some(obj) = value.and_then(|v| v.as_object()) else {
        return WorkerThreadOptions::default();
    };

    let resource_limits = obj
        .get("resourceLimits")
        .and_then(|v| v.as_object())
        .map(|rl| ResourceLimits {
            max_young_generation_size_mb: rl
                .get("maxYoungGenerationSizeMb")
                .and_then(|v| v.as_f64()),
            max_old_generation_size_mb: rl.get("maxOldGenerationSizeMb").and_then(|v| v.as_f64()),
            code_range_size_mb: rl.get("codeRangeSizeMb").and_then(|v| v.as_f64()),
            stack_size_mb: rl.get("stackSizeMb").and_then(|v| v.as_f64()),
        });

    WorkerThreadOptions {
        filename: obj
            .get("filename")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        worker_data: obj
            .get("workerData")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        eval: obj.get("eval").and_then(|v| v.as_bool()).unwrap_or(false),
        env: obj.get("env").and_then(|v| v.as_object()).map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        }),
        name: obj.get("name").and_then(|v| v.as_str()).map(String::from),
        resource_limits,
        track_unmanaged_fds: obj
            .get("trackUnmanagedFds")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        argv: obj
            .get("argv")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        exec_argv: obj
            .get("execArgv")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        stdin: obj.get("stdin").and_then(|v| v.as_bool()).unwrap_or(false),
        stdout: obj.get("stdout").and_then(|v| v.as_bool()).unwrap_or(false),
        stderr: obj.get("stderr").and_then(|v| v.as_bool()).unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let (ext, _) = extension();
        assert_eq!(ext.name(), "worker_threads");
        assert!(ext.js_code().is_some());
    }

    #[test]
    fn test_parse_worker_options_empty() {
        let options = parse_worker_options(None);
        assert!(options.filename.is_empty());
        assert!(!options.eval);
    }

    #[test]
    fn test_parse_worker_options_full() {
        let json = serde_json::json!({
            "filename": "worker.js",
            "workerData": {"key": "value"},
            "eval": true,
            "name": "test-worker",
            "resourceLimits": {
                "maxOldGenerationSizeMb": 128
            }
        });

        let options = parse_worker_options(Some(&json));
        assert_eq!(options.filename, "worker.js");
        assert!(options.eval);
        assert_eq!(options.name, Some("test-worker".to_string()));
        assert!(options.resource_limits.is_some());
        assert_eq!(
            options.resource_limits.unwrap().max_old_generation_size_mb,
            Some(128.0)
        );
    }
}
