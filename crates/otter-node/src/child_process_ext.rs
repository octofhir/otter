//! Child process extension module using the new architecture.
//!
//! This module provides the child_process extension for spawning child processes.
//!
//! ## Architecture
//!
//! - `child_process.rs` - Rust child process implementation
//! - `child_process_ext.rs` - Extension creation with ops
//! - `child_process.js` - JavaScript wrapper
//!
//! Note: This module uses shared state (ChildProcessManager) which doesn't fit the #[dive]
//! pattern, so we use traditional op_sync with closures.

use otter_runtime::Extension;
use otter_runtime::extension::{OpDecl, op_sync};
use serde_json::json;
use std::sync::Arc;

use crate::child_process::{ChildProcessEvent, ChildProcessManager, SpawnOptions, StdioConfig};

/// Create the child_process extension.
///
/// This extension provides Node.js-compatible child process spawning.
pub fn extension() -> Extension {
    let manager = Arc::new(ChildProcessManager::new());

    let mgr_spawn = manager.clone();
    let mgr_spawn_sync = manager.clone();
    let mgr_write = manager.clone();
    let mgr_close = manager.clone();
    let mgr_kill = manager.clone();
    let mgr_pid = manager.clone();
    let mgr_exit = manager.clone();
    let mgr_signal = manager.clone();
    let mgr_running = manager.clone();
    let mgr_killed = manager.clone();
    let mgr_ref = manager.clone();
    let mgr_unref = manager.clone();
    let mgr_poll = manager.clone();

    let mut ops: Vec<OpDecl> = Vec::new();

    // cpSpawn(command: string[], options?: object) -> id
    ops.push(op_sync("cpSpawn", move |_ctx, args| {
        let cmd_arr = args.first().and_then(|v| v.as_array()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("cpSpawn requires command array")
        })?;

        let command: Vec<String> = cmd_arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();

        let options = parse_spawn_options(args.get(1));

        let id = mgr_spawn
            .spawn(&command, options)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(id))
    }));

    // cpSpawnSync(command: string[], options?: object) -> result
    ops.push(op_sync("cpSpawnSync", move |_ctx, args| {
        let cmd_arr = args.first().and_then(|v| v.as_array()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("cpSpawnSync requires command array")
        })?;

        let command: Vec<String> = cmd_arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();

        let options = parse_spawn_options(args.get(1));
        let result = mgr_spawn_sync.spawn_sync(&command, options);

        Ok(json!({
            "pid": result.pid,
            "stdout": { "type": "Buffer", "data": result.stdout },
            "stderr": { "type": "Buffer", "data": result.stderr },
            "status": result.status,
            "signal": result.signal,
            "error": result.error,
        }))
    }));

    // cpWriteStdin(id: number, data: Buffer) -> null
    ops.push(op_sync("cpWriteStdin", move |_ctx, args| {
        let id =
            args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("cpWriteStdin requires id")
            })? as u32;

        let data = extract_buffer_data(args.get(1))?;

        mgr_write
            .write_stdin(id, data)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(null))
    }));

    // cpCloseStdin(id: number) -> null
    ops.push(op_sync("cpCloseStdin", move |_ctx, args| {
        let id =
            args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("cpCloseStdin requires id")
            })? as u32;

        mgr_close
            .close_stdin(id)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(null))
    }));

    // cpKill(id: number, signal?: string) -> boolean
    ops.push(op_sync("cpKill", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("cpKill requires id"))?
            as u32;

        let signal = args.get(1).and_then(|v| v.as_str());

        let result = mgr_kill
            .kill(id, signal)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(result))
    }));

    // cpPid(id: number) -> number | null
    ops.push(op_sync("cpPid", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("cpPid requires id"))?
            as u32;

        Ok(json!(mgr_pid.pid(id)))
    }));

    // cpExitCode(id: number) -> number | null
    ops.push(op_sync("cpExitCode", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("cpExitCode requires id"))?
            as u32;

        Ok(json!(mgr_exit.exit_code(id)))
    }));

    // cpSignalCode(id: number) -> string | null
    ops.push(op_sync("cpSignalCode", move |_ctx, args| {
        let id =
            args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("cpSignalCode requires id")
            })? as u32;

        Ok(json!(mgr_signal.signal_code(id)))
    }));

    // cpIsRunning(id: number) -> boolean
    ops.push(op_sync("cpIsRunning", move |_ctx, args| {
        let id =
            args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("cpIsRunning requires id")
            })? as u32;

        Ok(json!(mgr_running.is_running(id)))
    }));

    // cpIsKilled(id: number) -> boolean
    ops.push(op_sync("cpIsKilled", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("cpIsKilled requires id"))?
            as u32;

        Ok(json!(mgr_killed.is_killed(id)))
    }));

    // cpRef(id: number) -> null
    ops.push(op_sync("cpRef", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("cpRef requires id"))?
            as u32;

        mgr_ref.ref_process(id);
        Ok(json!(null))
    }));

    // cpUnref(id: number) -> null
    ops.push(op_sync("cpUnref", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("cpUnref requires id"))?
            as u32;

        mgr_unref.unref_process(id);
        Ok(json!(null))
    }));

    // cpPollEvents() -> array
    ops.push(op_sync("cpPollEvents", move |_ctx, _args| {
        let events = mgr_poll.poll_events();
        let json_events: Vec<serde_json::Value> = events
            .into_iter()
            .map(|(id, event)| match event {
                ChildProcessEvent::Spawn => {
                    json!({"id": id, "type": "spawn"})
                }
                ChildProcessEvent::Stdout(data) => {
                    json!({"id": id, "type": "stdout", "data": {"type": "Buffer", "data": data}})
                }
                ChildProcessEvent::Stderr(data) => {
                    json!({"id": id, "type": "stderr", "data": {"type": "Buffer", "data": data}})
                }
                ChildProcessEvent::Exit { code, signal } => {
                    json!({"id": id, "type": "exit", "code": code, "signal": signal})
                }
                ChildProcessEvent::Close { code, signal } => {
                    json!({"id": id, "type": "close", "code": code, "signal": signal})
                }
                ChildProcessEvent::Error(msg) => {
                    json!({"id": id, "type": "error", "message": msg})
                }
                ChildProcessEvent::Message(data) => {
                    json!({"id": id, "type": "message", "data": data})
                }
            })
            .collect();

        Ok(json!(json_events))
    }));

    Extension::new("child_process")
        .with_ops(ops)
        .with_js(include_str!("child_process.js"))
}

/// Parse spawn options from JSON value
fn parse_spawn_options(value: Option<&serde_json::Value>) -> SpawnOptions {
    let Some(obj) = value.and_then(|v| v.as_object()) else {
        return SpawnOptions::default();
    };

    let parse_stdio = |v: &serde_json::Value| -> StdioConfig {
        match v.as_str() {
            Some("pipe") => StdioConfig::Pipe,
            Some("ignore") => StdioConfig::Ignore,
            Some("inherit") => StdioConfig::Inherit,
            _ => StdioConfig::Pipe,
        }
    };

    SpawnOptions {
        cwd: obj.get("cwd").and_then(|v| v.as_str()).map(String::from),
        env: obj.get("env").and_then(|v| v.as_object()).map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        }),
        stdin: obj
            .get("stdin")
            .map(parse_stdio)
            .unwrap_or(StdioConfig::Pipe),
        stdout: obj
            .get("stdout")
            .map(parse_stdio)
            .unwrap_or(StdioConfig::Pipe),
        stderr: obj
            .get("stderr")
            .map(parse_stdio)
            .unwrap_or(StdioConfig::Pipe),
        shell: obj.get("shell").and_then(|v| {
            if v.as_bool() == Some(true) {
                Some("/bin/sh".to_string())
            } else {
                v.as_str().map(String::from)
            }
        }),
        timeout: obj.get("timeout").and_then(|v| v.as_u64()),
        detached: obj
            .get("detached")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        ipc: obj.get("ipc").and_then(|v| v.as_bool()).unwrap_or(false),
    }
}

/// Extract buffer data from JSON value
fn extract_buffer_data(
    value: Option<&serde_json::Value>,
) -> Result<Vec<u8>, otter_runtime::error::JscError> {
    let Some(v) = value else {
        return Ok(Vec::new());
    };

    // Handle Buffer object: { type: "Buffer", data: [...] }
    if let Some(obj) = v.as_object() {
        if obj.get("type").and_then(|t| t.as_str()) == Some("Buffer") {
            if let Some(data) = obj.get("data").and_then(|d| d.as_array()) {
                return Ok(data
                    .iter()
                    .filter_map(|b| b.as_u64().map(|n| n as u8))
                    .collect());
            }
        }
    }

    // Handle string
    if let Some(s) = v.as_str() {
        return Ok(s.as_bytes().to_vec());
    }

    // Handle array of bytes
    if let Some(arr) = v.as_array() {
        return Ok(arr
            .iter()
            .filter_map(|b| b.as_u64().map(|n| n as u8))
            .collect());
    }

    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension();
        assert_eq!(ext.name(), "child_process");
        assert!(ext.js_code().is_some());
    }
}
