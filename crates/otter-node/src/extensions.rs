//! JavaScript extensions for Node.js modules.
//!
//! These extensions expose the Rust implementations to JavaScript.

use crate::{buffer, crypto, fs, http_request, http_server, url, util, websocket};
use otter_engine::Capabilities;
use tokio::sync::mpsc::UnboundedSender;
use otter_runtime::extension::{Extension, OpDecl, op_async, op_sync};
use otter_runtime::memory::jsc_heap_stats;
use otter_runtime::RuntimeContextHandle;
use parking_lot::Mutex;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

// NOTE: create_path_extension has been migrated to path_ext.rs using #[dive] macros
// See path_ext.rs for the new implementation with cleaner architecture

/// Create the node:buffer extension.
///
/// This module provides Buffer class for binary data manipulation.
pub fn create_buffer_extension() -> Extension {
    let js_wrapper = r#"
(function() {
    'use strict';

    // Buffer is represented as: { type: 'Buffer', data: number[] }
    // This wrapper exposes a Node-like Buffer class and registers node:buffer.
    class Buffer {
        constructor(value) {
            if (value && value.type === 'Buffer' && Array.isArray(value.data)) {
                this.type = 'Buffer';
                this.data = value.data;
                return;
            }
            if (Array.isArray(value)) {
                this.type = 'Buffer';
                this.data = value.map((n) => n & 0xff);
                return;
            }
            const empty = alloc(0, 0);
            this.type = 'Buffer';
            this.data = empty.data;
        }

        static alloc(size, fill) {
            return new Buffer(alloc(size, fill ?? 0));
        }

        static from(data, encoding) {
            if (data && data.type === 'Buffer' && Array.isArray(data.data)) {
                return new Buffer(data);
            }
            if (data && data.data && Array.isArray(data.data)) {
                return new Buffer({ type: 'Buffer', data: data.data });
            }
            return new Buffer(from(data, encoding || 'utf8'));
        }

        static concat(list, totalLength) {
            const normalized = (list || []).map((v) => {
                if (v && v.type === 'Buffer') return v;
                if (v && v.data && Array.isArray(v.data)) return { type: 'Buffer', data: v.data };
                return Buffer.from(v);
            });
            return new Buffer(concat(normalized, totalLength));
        }

        static isBuffer(value) {
            return value && value.type === 'Buffer' && Array.isArray(value.data);
        }

        static byteLength(value, encoding) {
            return byteLength(value, encoding || 'utf8');
        }

        toString(encoding, start, end) {
            const len = this.data.length;
            const s = start ?? 0;
            const e = end ?? len;
            return toString(this, encoding || 'utf8', s, e);
        }

        slice(start, end) {
            return new Buffer(slice(this, start, end));
        }

        equals(other) {
            return equals(this, other);
        }

        compare(other) {
            return compare(this, other);
        }

        get length() {
            return this.data.length;
        }

        [Symbol.iterator]() {
            return this.data[Symbol.iterator]();
        }
    }

    globalThis.Buffer = Buffer;

    const bufferModule = { Buffer };
    bufferModule.default = bufferModule;

    if (globalThis.__registerModule) {
        globalThis.__registerModule('buffer', bufferModule);
        globalThis.__registerModule('node:buffer', bufferModule);
    }
})();
"#;

    Extension::new("Buffer").with_ops(vec![
        op_sync("alloc", |_ctx, args| {
            let size = args.first().and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let fill = args
                .get(1)
                .and_then(|v| v.as_u64())
                .map(|n| n as u8)
                .unwrap_or(0);
            let buf = buffer::Buffer::alloc(size, fill);
            Ok(json!({
                "type": "Buffer",
                "data": buf.as_bytes(),
            }))
        }),
        op_sync("from", |_ctx, args| {
            let data = args.first().ok_or_else(|| {
                otter_runtime::error::JscError::internal("Buffer.from requires data argument")
            })?;
            let encoding = args.get(1).and_then(|v| v.as_str()).unwrap_or("utf8");

            let buf = if let Some(s) = data.as_str() {
                buffer::Buffer::from_string(s, encoding)
                    .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?
            } else if let Some(arr) = data.as_array() {
                let bytes: Vec<u8> = arr
                    .iter()
                    .filter_map(|v| v.as_u64().map(|n| n as u8))
                    .collect();
                buffer::Buffer::from_bytes(&bytes)
            } else {
                buffer::Buffer::new(Vec::new())
            };

            Ok(json!({
                "type": "Buffer",
                "data": buf.as_bytes(),
            }))
        }),
        op_sync("concat", |_ctx, args| {
            let list = args.first().and_then(|v| v.as_array()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("Buffer.concat requires array")
            })?;

            let total_length = args.get(1).and_then(|v| v.as_u64()).map(|n| n as usize);

            let mut result = Vec::new();
            for buf in list {
                if let Some(data) = buf.get("data").and_then(|v| v.as_array()) {
                    for byte in data {
                        if let Some(n) = byte.as_u64() {
                            result.push(n as u8);
                        }
                    }
                }
            }

            if let Some(len) = total_length {
                result.truncate(len);
            }

            Ok(json!({
                "type": "Buffer",
                "data": result,
            }))
        }),
        op_sync("isBuffer", |_ctx, args| {
            let is_buffer = args
                .first()
                .and_then(|v| v.as_object())
                .and_then(|o| o.get("type"))
                .and_then(|v| v.as_str())
                == Some("Buffer");
            Ok(json!(is_buffer))
        }),
        op_sync("byteLength", |_ctx, args| {
            let data = args.first().ok_or_else(|| {
                otter_runtime::error::JscError::internal("byteLength requires argument")
            })?;
            let encoding = args.get(1).and_then(|v| v.as_str()).unwrap_or("utf8");

            let length = if let Some(s) = data.as_str() {
                buffer::Buffer::byte_length(s, encoding)
            } else if let Some(obj) = data.as_object() {
                obj.get("data")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0)
            } else {
                0
            };

            Ok(json!(length))
        }),
        op_sync("toString", |_ctx, args| {
            let buffer_obj = args.first().ok_or_else(|| {
                otter_runtime::error::JscError::internal("toString requires Buffer")
            })?;

            let data = buffer_obj
                .get("data")
                .and_then(|v| v.as_array())
                .ok_or_else(|| otter_runtime::error::JscError::internal("Invalid buffer"))?;

            let encoding = args.get(1).and_then(|v| v.as_str()).unwrap_or("utf8");
            let start = args
                .get(2)
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(0);
            let end = args
                .get(3)
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(data.len());

            let bytes: Vec<u8> = data[start..end.min(data.len())]
                .iter()
                .filter_map(|v| v.as_u64().map(|n| n as u8))
                .collect();

            let buf = buffer::Buffer::from_bytes(&bytes);
            Ok(json!(buf.to_string(encoding, 0, buf.len())))
        }),
        op_sync("slice", |_ctx, args| {
            let buffer_obj = args
                .first()
                .ok_or_else(|| otter_runtime::error::JscError::internal("slice requires Buffer"))?;

            let data = buffer_obj
                .get("data")
                .and_then(|v| v.as_array())
                .ok_or_else(|| otter_runtime::error::JscError::internal("Invalid buffer"))?;

            let bytes: Vec<u8> = data
                .iter()
                .filter_map(|v| v.as_u64().map(|n| n as u8))
                .collect();

            let buf = buffer::Buffer::from_bytes(&bytes);
            let start = args.get(1).and_then(|v| v.as_i64()).unwrap_or(0) as isize;
            let end = args
                .get(2)
                .and_then(|v| v.as_i64())
                .unwrap_or(buf.len() as i64) as isize;

            let sliced = buf.slice(start, end);
            Ok(json!({
                "type": "Buffer",
                "data": sliced.as_bytes(),
            }))
        }),
        op_sync("equals", |_ctx, args| {
            let buf1_data: Vec<u8> = args
                .first()
                .and_then(|v| v.get("data"))
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as u8))
                        .collect()
                })
                .unwrap_or_default();

            let buf2_data: Vec<u8> = args
                .get(1)
                .and_then(|v| v.get("data"))
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as u8))
                        .collect()
                })
                .unwrap_or_default();

            let buf1 = buffer::Buffer::from_bytes(&buf1_data);
            let buf2 = buffer::Buffer::from_bytes(&buf2_data);
            Ok(json!(buf1.equals(&buf2)))
        }),
        op_sync("compare", |_ctx, args| {
            let buf1_data: Vec<u8> = args
                .first()
                .and_then(|v| v.get("data"))
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as u8))
                        .collect()
                })
                .unwrap_or_default();

            let buf2_data: Vec<u8> = args
                .get(1)
                .and_then(|v| v.get("data"))
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as u8))
                        .collect()
                })
                .unwrap_or_default();

            let buf1 = buffer::Buffer::from_bytes(&buf1_data);
            let buf2 = buffer::Buffer::from_bytes(&buf2_data);
            Ok(json!(buf1.compare(&buf2)))
        }),
    ])
    .with_js(js_wrapper)
}

/// Create the node:fs extension with capability-based security.
///
/// Provides both sync methods (node:fs style) and async methods (node:fs/promises style).
/// All filesystem operations require appropriate permissions.
pub fn create_fs_extension(capabilities: Capabilities) -> Extension {
    let caps = Arc::new(capabilities);

    let mut ops: Vec<OpDecl> = Vec::new();

    // ============ SYNC METHODS (node:fs style) ============

    // readFileSync
    let caps_read_sync = caps.clone();
    ops.push(op_sync("readFileSync", move |_ctx, args| {
        let path = args.first().and_then(|v| v.as_str()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("readFileSync requires path")
        })?;

        let encoding = args.get(1).and_then(|v| {
            v.as_str()
                .or_else(|| v.get("encoding").and_then(|e| e.as_str()))
        });

        let path_buf = std::path::Path::new(path).to_path_buf();
        if !caps_read_sync.can_read(&path_buf) {
            return Err(otter_runtime::error::JscError::internal(format!(
                "Permission denied: read access to '{}'",
                path
            )));
        }

        let contents = std::fs::read(&path_buf).map_err(|e| {
            otter_runtime::error::JscError::internal(format!("Failed to read '{}': {}", path, e))
        })?;

        match encoding {
            Some("utf8") | Some("utf-8") => {
                let text = String::from_utf8(contents)
                    .map_err(|_| otter_runtime::error::JscError::internal("Invalid UTF-8"))?;
                Ok(json!(text))
            }
            _ => Ok(json!({
                "type": "Buffer",
                "data": contents,
            })),
        }
    }));

    // writeFileSync
    let caps_write_sync = caps.clone();
    ops.push(op_sync("writeFileSync", move |_ctx, args| {
        let path = args.first().and_then(|v| v.as_str()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("writeFileSync requires path")
        })?;

        let data = args.get(1).ok_or_else(|| {
            otter_runtime::error::JscError::internal("writeFileSync requires data")
        })?;

        let path_buf = std::path::Path::new(path).to_path_buf();
        if !caps_write_sync.can_write(&path_buf) {
            return Err(otter_runtime::error::JscError::internal(format!(
                "Permission denied: write access to '{}'",
                path
            )));
        }

        let bytes = if let Some(s) = data.as_str() {
            s.as_bytes().to_vec()
        } else if let Some(arr) = data.as_array() {
            arr.iter()
                .filter_map(|v| v.as_u64().map(|n| n as u8))
                .collect()
        } else if let Some(obj) = data.as_object() {
            obj.get("data")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as u8))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        std::fs::write(&path_buf, bytes).map_err(|e| {
            otter_runtime::error::JscError::internal(format!("Failed to write '{}': {}", path, e))
        })?;

        Ok(json!(null))
    }));

    // readdirSync
    let caps_readdir_sync = caps.clone();
    ops.push(op_sync("readdirSync", move |_ctx, args| {
        let path = args.first().and_then(|v| v.as_str()).unwrap_or(".");

        let path_buf = std::path::Path::new(path).to_path_buf();
        if !caps_readdir_sync.can_read(&path_buf) {
            return Err(otter_runtime::error::JscError::internal(format!(
                "Permission denied: read access to '{}'",
                path
            )));
        }

        let entries: Vec<String> = std::fs::read_dir(&path_buf)
            .map_err(|e| {
                otter_runtime::error::JscError::internal(format!(
                    "Failed to read dir '{}': {}",
                    path, e
                ))
            })?
            .filter_map(|entry| {
                entry
                    .ok()
                    .map(|e| e.file_name().to_string_lossy().to_string())
            })
            .collect();

        Ok(json!(entries))
    }));

    // statSync
    let caps_stat_sync = caps.clone();
    ops.push(op_sync("statSync", move |_ctx, args| {
        let path = args
            .first()
            .and_then(|v| v.as_str())
            .ok_or_else(|| otter_runtime::error::JscError::internal("statSync requires path"))?;

        let path_buf = std::path::Path::new(path).to_path_buf();
        if !caps_stat_sync.can_read(&path_buf) {
            return Err(otter_runtime::error::JscError::internal(format!(
                "Permission denied: read access to '{}'",
                path
            )));
        }

        let metadata = std::fs::metadata(&path_buf).map_err(|e| {
            otter_runtime::error::JscError::internal(format!("Failed to stat '{}': {}", path, e))
        })?;

        let file_type = metadata.file_type();

        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::MetadataExt;
            metadata.mode()
        };
        #[cfg(not(unix))]
        let mode = 0u32;

        Ok(json!({
            "isFile": file_type.is_file(),
            "isDirectory": file_type.is_dir(),
            "isSymbolicLink": file_type.is_symlink(),
            "size": metadata.len(),
            "mode": mode,
        }))
    }));

    // mkdirSync
    let caps_mkdir_sync = caps.clone();
    ops.push(op_sync("mkdirSync", move |_ctx, args| {
        let path = args
            .first()
            .and_then(|v| v.as_str())
            .ok_or_else(|| otter_runtime::error::JscError::internal("mkdirSync requires path"))?;

        let path_buf = std::path::Path::new(path).to_path_buf();
        if !caps_mkdir_sync.can_write(&path_buf) {
            return Err(otter_runtime::error::JscError::internal(format!(
                "Permission denied: write access to '{}'",
                path
            )));
        }

        let recursive = args
            .get(1)
            .and_then(|v| v.get("recursive"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if recursive {
            std::fs::create_dir_all(&path_buf)
        } else {
            std::fs::create_dir(&path_buf)
        }
        .map_err(|e| {
            otter_runtime::error::JscError::internal(format!(
                "Failed to create dir '{}': {}",
                path, e
            ))
        })?;

        Ok(json!(null))
    }));

    // rmSync
    let caps_rm_sync = caps.clone();
    ops.push(op_sync("rmSync", move |_ctx, args| {
        let path = args
            .first()
            .and_then(|v| v.as_str())
            .ok_or_else(|| otter_runtime::error::JscError::internal("rmSync requires path"))?;

        let path_buf = std::path::Path::new(path).to_path_buf();
        if !caps_rm_sync.can_write(&path_buf) {
            return Err(otter_runtime::error::JscError::internal(format!(
                "Permission denied: write access to '{}'",
                path
            )));
        }

        let recursive = args
            .get(1)
            .and_then(|v| v.get("recursive"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let metadata = std::fs::metadata(&path_buf).map_err(|e| {
            otter_runtime::error::JscError::internal(format!("Failed to stat '{}': {}", path, e))
        })?;

        if metadata.is_dir() && recursive {
            std::fs::remove_dir_all(&path_buf)
        } else if metadata.is_dir() {
            std::fs::remove_dir(&path_buf)
        } else {
            std::fs::remove_file(&path_buf)
        }
        .map_err(|e| {
            otter_runtime::error::JscError::internal(format!("Failed to remove '{}': {}", path, e))
        })?;

        Ok(json!(null))
    }));

    // existsSync
    let caps_exists_sync = caps.clone();
    ops.push(op_sync("existsSync", move |_ctx, args| {
        let path = args.first().and_then(|v| v.as_str()).unwrap_or("");

        let path_buf = std::path::Path::new(path).to_path_buf();
        if !caps_exists_sync.can_read(&path_buf) {
            return Err(otter_runtime::error::JscError::internal(format!(
                "Permission denied: read access to '{}'",
                path
            )));
        }

        Ok(json!(path_buf.exists()))
    }));

    // copyFileSync
    let caps_copy_sync = caps.clone();
    ops.push(op_sync("copyFileSync", move |_ctx, args| {
        let src = args
            .first()
            .and_then(|v| v.as_str())
            .ok_or_else(|| otter_runtime::error::JscError::internal("copyFileSync requires src"))?;

        let dest = args.get(1).and_then(|v| v.as_str()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("copyFileSync requires dest")
        })?;

        let src_path = std::path::Path::new(src).to_path_buf();
        let dest_path = std::path::Path::new(dest).to_path_buf();

        if !caps_copy_sync.can_read(&src_path) {
            return Err(otter_runtime::error::JscError::internal(format!(
                "Permission denied: read access to '{}'",
                src
            )));
        }

        if !caps_copy_sync.can_write(&dest_path) {
            return Err(otter_runtime::error::JscError::internal(format!(
                "Permission denied: write access to '{}'",
                dest
            )));
        }

        let bytes = std::fs::copy(&src_path, &dest_path).map_err(|e| {
            otter_runtime::error::JscError::internal(format!(
                "Failed to copy '{}' to '{}': {}",
                src, dest, e
            ))
        })?;

        Ok(json!(bytes))
    }));

    // ============ ASYNC METHODS (node:fs/promises style) ============

    // readFile
    let caps_read = caps.clone();
    ops.push(op_async("readFile", move |_ctx, args| {
        let caps = caps_read.clone();
        async move {
            let path = args.first().and_then(|v| v.as_str()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("readFile requires path")
            })?;

            let encoding = args.get(1).and_then(|v| {
                v.as_str()
                    .or_else(|| v.get("encoding").and_then(|e| e.as_str()))
            });

            let result = fs::read_file(&caps, path, encoding)
                .await
                .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

            match result {
                fs::ReadResult::String(s) => Ok(json!(s)),
                fs::ReadResult::Bytes(bytes) => Ok(json!({
                    "type": "Buffer",
                    "data": bytes,
                })),
            }
        }
    }));

    // writeFile
    let caps_write = caps.clone();
    ops.push(op_async("writeFile", move |_ctx, args| {
        let caps = caps_write.clone();
        async move {
            let path = args.first().and_then(|v| v.as_str()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("writeFile requires path")
            })?;

            let data = args.get(1).ok_or_else(|| {
                otter_runtime::error::JscError::internal("writeFile requires data")
            })?;

            let bytes = if let Some(s) = data.as_str() {
                s.as_bytes().to_vec()
            } else if let Some(arr) = data.as_array() {
                arr.iter()
                    .filter_map(|v| v.as_u64().map(|n| n as u8))
                    .collect()
            } else if let Some(obj) = data.as_object() {
                obj.get("data")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

            fs::write_file(&caps, path, &bytes)
                .await
                .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

            Ok(json!(null))
        }
    }));

    // readdir
    let caps_readdir = caps.clone();
    ops.push(op_async("readdir", move |_ctx, args| {
        let caps = caps_readdir.clone();
        async move {
            let path = args.first().and_then(|v| v.as_str()).unwrap_or(".");

            let entries = fs::readdir(&caps, path)
                .await
                .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

            Ok(json!(entries))
        }
    }));

    // stat
    let caps_stat = caps.clone();
    ops.push(op_async("stat", move |_ctx, args| {
        let caps = caps_stat.clone();
        async move {
            let path = args
                .first()
                .and_then(|v| v.as_str())
                .ok_or_else(|| otter_runtime::error::JscError::internal("stat requires path"))?;

            let stats = fs::stat(&caps, path)
                .await
                .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

            Ok(json!({
                "isFile": stats.is_file,
                "isDirectory": stats.is_directory,
                "isSymbolicLink": stats.is_symlink,
                "size": stats.size,
                "mode": stats.mode,
                "mtimeMs": stats.mtime_ms,
                "atimeMs": stats.atime_ms,
                "ctimeMs": stats.ctime_ms,
            }))
        }
    }));

    // mkdir
    let caps_mkdir = caps.clone();
    ops.push(op_async("mkdir", move |_ctx, args| {
        let caps = caps_mkdir.clone();
        async move {
            let path = args
                .first()
                .and_then(|v| v.as_str())
                .ok_or_else(|| otter_runtime::error::JscError::internal("mkdir requires path"))?;

            let recursive = args
                .get(1)
                .and_then(|v| v.get("recursive"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            fs::mkdir(&caps, path, recursive)
                .await
                .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

            Ok(json!(null))
        }
    }));

    // rm
    let caps_rm = caps.clone();
    ops.push(op_async("rm", move |_ctx, args| {
        let caps = caps_rm.clone();
        async move {
            let path = args
                .first()
                .and_then(|v| v.as_str())
                .ok_or_else(|| otter_runtime::error::JscError::internal("rm requires path"))?;

            let recursive = args
                .get(1)
                .and_then(|v| v.get("recursive"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            fs::rm(&caps, path, recursive)
                .await
                .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

            Ok(json!(null))
        }
    }));

    // exists
    let caps_exists = caps.clone();
    ops.push(op_async("exists", move |_ctx, args| {
        let caps = caps_exists.clone();
        async move {
            let path = args.first().and_then(|v| v.as_str()).unwrap_or("");

            let exists = fs::exists(&caps, path)
                .await
                .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

            Ok(json!(exists))
        }
    }));

    // rename
    let caps_rename = caps.clone();
    ops.push(op_async("rename", move |_ctx, args| {
        let caps = caps_rename.clone();
        async move {
            let old_path = args.first().and_then(|v| v.as_str()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("rename requires oldPath")
            })?;

            let new_path = args.get(1).and_then(|v| v.as_str()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("rename requires newPath")
            })?;

            fs::rename(&caps, old_path, new_path)
                .await
                .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

            Ok(json!(null))
        }
    }));

    // copyFile
    let caps_copy = caps.clone();
    ops.push(op_async("copyFile", move |_ctx, args| {
        let caps = caps_copy.clone();
        async move {
            let src = args
                .first()
                .and_then(|v| v.as_str())
                .ok_or_else(|| otter_runtime::error::JscError::internal("copyFile requires src"))?;

            let dest = args.get(1).and_then(|v| v.as_str()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("copyFile requires dest")
            })?;

            let bytes = fs::copy_file(&caps, src, dest)
                .await
                .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

            Ok(json!(bytes))
        }
    }));

    let js_wrapper = r#"
(function() {
    'use strict';

    function callbackify(promiseFn) {
        return function(...args) {
            const cb = args.length && typeof args[args.length - 1] === 'function'
                ? args.pop()
                : null;

            if (!cb) return promiseFn(...args);

            promiseFn(...args).then(
                (value) => cb(null, value),
                (err) => cb(err)
            );
        };
    }

    const fsPromises = {
        readFile: (...args) => readFile(...args),
        writeFile: (...args) => writeFile(...args),
        readdir: (...args) => readdir(...args),
        stat: (...args) => stat(...args),
        mkdir: (...args) => mkdir(...args),
        rm: (...args) => rm(...args),
        exists: (...args) => exists(...args),
        rename: (...args) => rename(...args),
        copyFile: (...args) => copyFile(...args),
    };
    fsPromises.default = fsPromises;

    const fsModule = {
        readFileSync: (...args) => readFileSync(...args),
        writeFileSync: (...args) => writeFileSync(...args),
        readdirSync: (...args) => readdirSync(...args),
        statSync: (...args) => statSync(...args),
        mkdirSync: (...args) => mkdirSync(...args),
        rmSync: (...args) => rmSync(...args),
        existsSync: (...args) => existsSync(...args),
        copyFileSync: (...args) => copyFileSync(...args),

        readFile: callbackify(readFile),
        writeFile: callbackify(writeFile),
        readdir: callbackify(readdir),
        stat: callbackify(stat),
        mkdir: callbackify(mkdir),
        rm: callbackify(rm),
        exists: callbackify(exists),
        rename: callbackify(rename),
        copyFile: callbackify(copyFile),

        promises: fsPromises,
    };
    fsModule.default = fsModule;

    if (globalThis.__registerModule) {
        globalThis.__registerModule('fs', fsModule);
        globalThis.__registerModule('node:fs', fsModule);
        globalThis.__registerModule('node:fs/promises', fsPromises);
    }
})();
"#;

    Extension::new("fs").with_ops(ops).with_js(js_wrapper)
}

/// Create the node:test extension for test running.
///
/// Provides test runner functionality including describe, it, test, and assertions.
pub fn create_test_extension() -> Extension {
    use crate::test::{TestSummary, create_test_runner};

    let runner = create_test_runner();

    let mut ops: Vec<OpDecl> = Vec::new();

    // Internal ops (prefixed with __) - used by JS wrapper
    // __startSuite(name) - start a test suite
    let runner_describe = runner.clone();
    ops.push(op_sync("__startSuite", move |_ctx, args| {
        let name = args.first().and_then(|v| v.as_str()).unwrap_or("anonymous");
        runner_describe.lock().unwrap().start_suite(name);
        Ok(json!(null))
    }));

    // __endSuite() - end current test suite
    let runner_end = runner.clone();
    ops.push(op_sync("__endSuite", move |_ctx, _args| {
        runner_end.lock().unwrap().end_suite();
        Ok(json!(null))
    }));

    // __recordResult(name, passed, duration, error?) - record a test result
    let runner_record = runner.clone();
    ops.push(op_sync("__recordResult", move |_ctx, args| {
        let name = args.first().and_then(|v| v.as_str()).unwrap_or("");
        let passed = args.get(1).and_then(|v| v.as_bool()).unwrap_or(false);
        let duration = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0);
        let error = args.get(3).and_then(|v| v.as_str()).map(|s| s.to_string());

        runner_record
            .lock()
            .unwrap()
            .record_test(name, passed, duration, error);
        Ok(json!(null))
    }));

    // __skip(name) - skip a test
    let runner_skip = runner.clone();
    ops.push(op_sync("__skipTest", move |_ctx, args| {
        let name = args.first().and_then(|v| v.as_str()).unwrap_or("");
        runner_skip.lock().unwrap().skip_test(name);
        Ok(json!(null))
    }));

    // __getSummary() - get test results summary
    let runner_summary = runner.clone();
    ops.push(op_sync("__getSummary", move |_ctx, _args| {
        let runner = runner_summary.lock().unwrap();
        let summary = TestSummary::from(&*runner);
        Ok(serde_json::to_value(summary).unwrap_or(json!(null)))
    }));

    // __resetTests() - reset test runner for a new run
    let runner_reset = runner.clone();
    ops.push(op_sync("__resetTests", move |_ctx, _args| {
        runner_reset.lock().unwrap().reset();
        Ok(json!(null))
    }));

    // assertEqual(actual, expected) - assert two values are equal
    ops.push(op_sync("assertEqual", |_ctx, args| {
        let actual = args.first();
        let expected = args.get(1);

        if actual == expected {
            Ok(json!(true))
        } else {
            Err(otter_runtime::error::JscError::internal(format!(
                "Assertion failed: {:?} !== {:?}",
                actual, expected
            )))
        }
    }));

    // assertNotEqual(actual, expected) - assert two values are not equal
    ops.push(op_sync("assertNotEqual", |_ctx, args| {
        let actual = args.first();
        let expected = args.get(1);

        if actual != expected {
            Ok(json!(true))
        } else {
            Err(otter_runtime::error::JscError::internal(format!(
                "Assertion failed: {:?} === {:?} (expected not equal)",
                actual, expected
            )))
        }
    }));

    // assertTrue(value) - assert value is truthy
    ops.push(op_sync("assertTrue", |_ctx, args| {
        let value = args.first();
        let is_truthy = match value {
            Some(v) => {
                !v.is_null()
                    && v.as_bool() != Some(false)
                    && v.as_i64() != Some(0)
                    && v.as_str() != Some("")
            }
            None => false,
        };

        if is_truthy {
            Ok(json!(true))
        } else {
            Err(otter_runtime::error::JscError::internal(
                "Assertion failed: expected truthy value",
            ))
        }
    }));

    // assertFalse(value) - assert value is falsy
    ops.push(op_sync("assertFalse", |_ctx, args| {
        let value = args.first();
        let is_falsy = match value {
            Some(v) => {
                v.is_null()
                    || v.as_bool() == Some(false)
                    || v.as_i64() == Some(0)
                    || v.as_str() == Some("")
            }
            None => true,
        };

        if is_falsy {
            Ok(json!(true))
        } else {
            Err(otter_runtime::error::JscError::internal(
                "Assertion failed: expected falsy value",
            ))
        }
    }));

    // assertOk(value) - assert value exists and is truthy
    ops.push(op_sync("assertOk", |_ctx, args| {
        let value = args.first();

        match value {
            Some(v) if !v.is_null() => Ok(json!(true)),
            _ => Err(otter_runtime::error::JscError::internal(
                "Assertion failed: expected ok value",
            )),
        }
    }));

    // assertDeepEqual(actual, expected) - deep equality check via JSON
    ops.push(op_sync("assertDeepEqual", |_ctx, args| {
        let actual = args.first();
        let expected = args.get(1);

        // Compare JSON representations
        let actual_str = actual.map(|v| v.to_string()).unwrap_or_default();
        let expected_str = expected.map(|v| v.to_string()).unwrap_or_default();

        if actual_str == expected_str {
            Ok(json!(true))
        } else {
            Err(otter_runtime::error::JscError::internal(format!(
                "Deep equal assertion failed:\n  actual: {}\n  expected: {}",
                actual_str, expected_str
            )))
        }
    }));

    // JavaScript wrapper that provides full node:test API
    let js_wrapper = r#"
// node:test wrapper - provides describe, it, test, and assertion APIs

(function() {
    'use strict';

    // Test queue and state
    const testQueue = [];
    let currentSuite = null;
    let hasOnly = false;

    // describe - create a test suite
    globalThis.describe = function describe(name, fn) {
        const suite = {
            type: 'suite',
            name: name,
            tests: [],
            beforeAll: null,
            afterAll: null,
            beforeEach: null,
            afterEach: null
        };

        const prevSuite = currentSuite;
        currentSuite = suite;

        // Execute the callback to collect tests
        fn();

        currentSuite = prevSuite;

        if (prevSuite) {
            prevSuite.tests.push(suite);
        } else {
            testQueue.push(suite);
        }
    };

    // it / test - define a test
    globalThis.it = function it(name, fn) {
        const testCase = {
            type: 'test',
            name: name,
            fn: fn,
            skip: false,
            only: false
        };

        if (currentSuite) {
            currentSuite.tests.push(testCase);
        } else {
            testQueue.push(testCase);
        }
    };

    globalThis.test = globalThis.it;

    // it.skip - skip a test
    globalThis.it.skip = function skip(name, fn) {
        const testCase = {
            type: 'test',
            name: name,
            fn: fn,
            skip: true,
            only: false
        };

        if (currentSuite) {
            currentSuite.tests.push(testCase);
        } else {
            testQueue.push(testCase);
        }
    };

    globalThis.test.skip = globalThis.it.skip;

    // it.only - run only this test
    globalThis.it.only = function only(name, fn) {
        hasOnly = true;
        const testCase = {
            type: 'test',
            name: name,
            fn: fn,
            skip: false,
            only: true
        };

        if (currentSuite) {
            currentSuite.tests.push(testCase);
        } else {
            testQueue.push(testCase);
        }
    };

    globalThis.test.only = globalThis.it.only;

    // describe.skip - skip a suite
    globalThis.describe.skip = function skip(name, fn) {
        const suite = {
            type: 'suite',
            name: name,
            tests: [],
            skip: true
        };
        if (currentSuite) {
            currentSuite.tests.push(suite);
        } else {
            testQueue.push(suite);
        }
    };

    // describe.only - run only this suite
    globalThis.describe.only = function only(name, fn) {
        hasOnly = true;
        const suite = {
            type: 'suite',
            name: name,
            tests: [],
            only: true,
            beforeAll: null,
            afterAll: null,
            beforeEach: null,
            afterEach: null
        };

        const prevSuite = currentSuite;
        currentSuite = suite;
        fn();
        currentSuite = prevSuite;

        if (prevSuite) {
            prevSuite.tests.push(suite);
        } else {
            testQueue.push(suite);
        }
    };

    // Hook functions
    globalThis.beforeEach = function beforeEach(fn) {
        if (currentSuite) {
            currentSuite.beforeEach = fn;
        }
    };

    globalThis.afterEach = function afterEach(fn) {
        if (currentSuite) {
            currentSuite.afterEach = fn;
        }
    };

    globalThis.before = function before(fn) {
        if (currentSuite) {
            currentSuite.beforeAll = fn;
        }
    };

    globalThis.after = function after(fn) {
        if (currentSuite) {
            currentSuite.afterAll = fn;
        }
    };

    // Check if any test in the tree has .only
    function checkHasOnly(items) {
        for (const item of items) {
            if (item.only) return true;
            if (item.type === 'suite' && item.tests) {
                if (checkHasOnly(item.tests)) return true;
            }
        }
        return false;
    }

    // Run a single test (async version - supports async test functions)
    async function runTest(test, suitePath, hooks) {
        const fullName = suitePath ? suitePath + ' > ' + test.name : test.name;

        // Check if should skip
        if (test.skip) {
            __skipTest(test.name);
            console.log('  - ' + fullName + ' (skipped)');
            return;
        }

        // Check if we have .only tests and this isn't one
        if (hasOnly && !test.only) {
            __skipTest(test.name);
            console.log('  - ' + fullName + ' (skipped)');
            return;
        }

        const start = Date.now();
        __startSuite(test.name);

        try {
            // Run beforeEach hooks
            if (hooks.beforeEach) {
                await hooks.beforeEach();
            }

            // Run the test (await in case it's async)
            await test.fn();

            // Run afterEach hooks
            if (hooks.afterEach) {
                await hooks.afterEach();
            }

            const duration = Date.now() - start;
            __recordResult(test.name, true, duration, null);
            console.log('  ✓ ' + fullName + ' (' + duration + 'ms)');
        } catch (error) {
            const duration = Date.now() - start;
            const errorMsg = error && error.message ? error.message : String(error);
            __recordResult(test.name, false, duration, errorMsg);
            console.log('  ✗ ' + fullName + ' (' + duration + 'ms)');
            console.log('    ' + errorMsg);
        }

        __endSuite();
    }

    // Run a test suite (async version)
    async function runSuite(suite, parentPath, parentHooks) {
        const suitePath = parentPath ? parentPath + ' > ' + suite.name : suite.name;

        // Check if should skip entire suite
        if (suite.skip) {
            console.log('\n' + suitePath + ' (skipped)');
            for (const item of (suite.tests || [])) {
                if (item.type === 'test') {
                    __skipTest(item.name);
                }
            }
            return;
        }

        console.log('\n' + suitePath);
        __startSuite(suite.name);

        // Merge hooks
        const hooks = {
            beforeEach: suite.beforeEach || parentHooks.beforeEach,
            afterEach: suite.afterEach || parentHooks.afterEach
        };

        // Run beforeAll
        if (suite.beforeAll) {
            try {
                await suite.beforeAll();
            } catch (error) {
                console.log('  beforeAll failed: ' + (error.message || error));
            }
        }

        // Run tests and nested suites
        for (const item of (suite.tests || [])) {
            if (item.type === 'suite') {
                await runSuite(item, suitePath, hooks);
            } else {
                await runTest(item, suitePath, hooks);
            }
        }

        // Run afterAll
        if (suite.afterAll) {
            try {
                await suite.afterAll();
            } catch (error) {
                console.log('  afterAll failed: ' + (error.message || error));
            }
        }

        __endSuite();
    }

    // run - execute all queued tests (async version)
    globalThis.run = async function run() {
        // Reset runner state
        __resetTests();

        // Check for .only tests
        hasOnly = checkHasOnly(testQueue);

        console.log('Running tests...');

        for (const item of testQueue) {
            if (item.type === 'suite') {
                await runSuite(item, '', {});
            } else {
                await runTest(item, '', {});
            }
        }

        const summary = __getSummary();

        console.log('\n' + summary.passed + ' passing');
        if (summary.failed > 0) {
            console.log(summary.failed + ' failing');
        }
        if (summary.skipped > 0) {
            console.log(summary.skipped + ' skipped');
        }

        // Clear queue for next run
        testQueue.length = 0;
        hasOnly = false;

        return summary;
    };

    // assert - assertion utilities
    globalThis.assert = {
        equal: function(actual, expected) {
            assertEqual(actual, expected);
        },
        strictEqual: function(actual, expected) {
            assertEqual(actual, expected);
        },
        notEqual: function(actual, expected) {
            assertNotEqual(actual, expected);
        },
        ok: function(value) {
            assertOk(value);
        },
        true: function(value) {
            assertTrue(value);
        },
        false: function(value) {
            assertFalse(value);
        },
        deepEqual: function(actual, expected) {
            assertDeepEqual(actual, expected);
        },
        throws: async function(fn, expected) {
            let threw = false;
            let error = null;
            try {
                await fn();
            } catch (e) {
                threw = true;
                error = e;
            }
            if (!threw) {
                throw new Error('Expected function to throw');
            }
            if (expected) {
                const msg = error && error.message ? error.message : String(error);
                if (typeof expected === 'string' && !msg.includes(expected)) {
                    throw new Error('Expected error "' + expected + '", got "' + msg + '"');
                }
                if (expected instanceof RegExp && !expected.test(msg)) {
                    throw new Error('Expected error matching ' + expected + ', got "' + msg + '"');
                }
            }
        },
        isNull: function(value) {
            if (value !== null && value !== undefined) {
                throw new Error('Expected null or undefined, got ' + typeof value);
            }
        },
        isNotNull: function(value) {
            if (value === null || value === undefined) {
                throw new Error('Expected non-null value');
            }
        }
    };

    const testModule = {
        describe: globalThis.describe,
        it: globalThis.it,
        test: globalThis.test,
        run: globalThis.run,
        assert: globalThis.assert,
    };
    testModule.default = testModule;

    if (globalThis.__registerModule) {
        globalThis.__registerModule('test', testModule);
        globalThis.__registerModule('node:test', testModule);
    }
})();
"#;

    Extension::new("test").with_ops(ops).with_js(js_wrapper)
}

/// Create the node:crypto extension.
///
/// Provides cryptographic functionality compatible with Node.js.
pub fn create_crypto_extension() -> Extension {
    // Shared state for incremental hashing
    let hash_contexts: Arc<Mutex<std::collections::HashMap<u32, crypto::Hash>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    let hmac_contexts: Arc<Mutex<std::collections::HashMap<u32, crypto::Hmac>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    let next_id = Arc::new(std::sync::atomic::AtomicU32::new(1));

    let mut ops: Vec<OpDecl> = Vec::new();

    // randomBytes(size) -> Buffer
    ops.push(op_sync("randomBytes", |_ctx, args| {
        let size = args.first().and_then(|v| v.as_u64()).unwrap_or(0) as usize;

        let bytes = crypto::random_bytes(size)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!({
            "type": "Buffer",
            "data": bytes,
        }))
    }));

    // randomUUID() -> string
    ops.push(op_sync("randomUUID", |_ctx, _args| {
        Ok(json!(crypto::random_uuid()))
    }));

    // getRandomValues(length) -> array of random bytes
    ops.push(op_sync("getRandomValues", |_ctx, args| {
        let length = args.first().and_then(|v| v.as_u64()).unwrap_or(0) as usize;

        let bytes = crypto::random_bytes(length)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(bytes))
    }));

    // createHash(algorithm) -> hash_id
    let hash_ctx = hash_contexts.clone();
    let hash_id = next_id.clone();
    ops.push(op_sync("createHash", move |_ctx, args| {
        let algorithm = args.first().and_then(|v| v.as_str()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("createHash requires algorithm")
        })?;

        let hash = crypto::create_hash(algorithm)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        let id = hash_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        hash_ctx.lock().insert(id, hash);

        Ok(json!(id))
    }));

    // hashUpdate(id, data) -> null
    let hash_ctx_update = hash_contexts.clone();
    ops.push(op_sync("hashUpdate", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("hashUpdate requires id"))?
            as u32;

        let data = args
            .get(1)
            .ok_or_else(|| otter_runtime::error::JscError::internal("hashUpdate requires data"))?;

        let bytes: Vec<u8> = if let Some(s) = data.as_str() {
            s.as_bytes().to_vec()
        } else if let Some(arr) = data.as_array() {
            arr.iter()
                .filter_map(|v| v.as_u64().map(|n| n as u8))
                .collect()
        } else if let Some(obj) = data.as_object() {
            obj.get("data")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as u8))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let mut contexts = hash_ctx_update.lock();
        let hash = contexts
            .get_mut(&id)
            .ok_or_else(|| otter_runtime::error::JscError::internal("Invalid hash id"))?;

        hash.update(&bytes);
        Ok(json!(null))
    }));

    // hashDigest(id, encoding) -> string or Buffer
    let hash_ctx_digest = hash_contexts.clone();
    ops.push(op_sync("hashDigest", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("hashDigest requires id"))?
            as u32;

        let encoding = args.get(1).and_then(|v| v.as_str());

        let mut contexts = hash_ctx_digest.lock();
        let hash = contexts
            .remove(&id)
            .ok_or_else(|| otter_runtime::error::JscError::internal("Invalid hash id"))?;

        let digest = hash.digest();

        match encoding {
            Some("hex") => Ok(json!(crypto::to_hex(&digest))),
            Some("base64") => Ok(json!(crypto::to_base64(&digest))),
            _ => Ok(json!({
                "type": "Buffer",
                "data": digest,
            })),
        }
    }));

    // createHmac(algorithm, key) -> hmac_id
    let hmac_ctx = hmac_contexts.clone();
    let hmac_id = next_id.clone();
    ops.push(op_sync("createHmac", move |_ctx, args| {
        let algorithm = args.first().and_then(|v| v.as_str()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("createHmac requires algorithm")
        })?;

        let key_arg = args
            .get(1)
            .ok_or_else(|| otter_runtime::error::JscError::internal("createHmac requires key"))?;

        let key: Vec<u8> = if let Some(s) = key_arg.as_str() {
            s.as_bytes().to_vec()
        } else if let Some(arr) = key_arg.as_array() {
            arr.iter()
                .filter_map(|v| v.as_u64().map(|n| n as u8))
                .collect()
        } else if let Some(obj) = key_arg.as_object() {
            obj.get("data")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as u8))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let hmac = crypto::create_hmac(algorithm, &key)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        let id = hmac_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        hmac_ctx.lock().insert(id, hmac);

        Ok(json!(id))
    }));

    // hmacUpdate(id, data) -> null
    let hmac_ctx_update = hmac_contexts.clone();
    ops.push(op_sync("hmacUpdate", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("hmacUpdate requires id"))?
            as u32;

        let data = args
            .get(1)
            .ok_or_else(|| otter_runtime::error::JscError::internal("hmacUpdate requires data"))?;

        let bytes: Vec<u8> = if let Some(s) = data.as_str() {
            s.as_bytes().to_vec()
        } else if let Some(arr) = data.as_array() {
            arr.iter()
                .filter_map(|v| v.as_u64().map(|n| n as u8))
                .collect()
        } else if let Some(obj) = data.as_object() {
            obj.get("data")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as u8))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let mut contexts = hmac_ctx_update.lock();
        let hmac = contexts
            .get_mut(&id)
            .ok_or_else(|| otter_runtime::error::JscError::internal("Invalid hmac id"))?;

        hmac.update(&bytes);
        Ok(json!(null))
    }));

    // hmacDigest(id, encoding) -> string or Buffer
    let hmac_ctx_digest = hmac_contexts.clone();
    ops.push(op_sync("hmacDigest", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("hmacDigest requires id"))?
            as u32;

        let encoding = args.get(1).and_then(|v| v.as_str());

        let mut contexts = hmac_ctx_digest.lock();
        let hmac = contexts
            .remove(&id)
            .ok_or_else(|| otter_runtime::error::JscError::internal("Invalid hmac id"))?;

        let digest = hmac.digest();

        match encoding {
            Some("hex") => Ok(json!(crypto::to_hex(&digest))),
            Some("base64") => Ok(json!(crypto::to_base64(&digest))),
            _ => Ok(json!({
                "type": "Buffer",
                "data": digest,
            })),
        }
    }));

    // hash(algorithm, data, encoding) -> one-shot hash
    ops.push(op_sync("hash", |_ctx, args| {
        let algorithm = args
            .first()
            .and_then(|v| v.as_str())
            .ok_or_else(|| otter_runtime::error::JscError::internal("hash requires algorithm"))?;

        let data = args
            .get(1)
            .ok_or_else(|| otter_runtime::error::JscError::internal("hash requires data"))?;

        let encoding = args.get(2).and_then(|v| v.as_str());

        let bytes: Vec<u8> = if let Some(s) = data.as_str() {
            s.as_bytes().to_vec()
        } else if let Some(arr) = data.as_array() {
            arr.iter()
                .filter_map(|v| v.as_u64().map(|n| n as u8))
                .collect()
        } else if let Some(obj) = data.as_object() {
            obj.get("data")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as u8))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let digest = crypto::hash(algorithm, &bytes)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        match encoding {
            Some("hex") => Ok(json!(crypto::to_hex(&digest))),
            Some("base64") => Ok(json!(crypto::to_base64(&digest))),
            _ => Ok(json!({
                "type": "Buffer",
                "data": digest,
            })),
        }
    }));

    // JavaScript wrapper that provides the full crypto API
    let js_wrapper = r#"
// node:crypto wrapper - provides createHash, createHmac, randomBytes, etc.

(function() {
    'use strict';

    // Hash class
    class Hash {
        constructor(id) {
            this._id = id;
        }

        update(data) {
            hashUpdate(this._id, data);
            return this;
        }

        digest(encoding) {
            return hashDigest(this._id, encoding);
        }
    }

    // Hmac class
    class Hmac {
        constructor(id) {
            this._id = id;
        }

        update(data) {
            hmacUpdate(this._id, data);
            return this;
        }

        digest(encoding) {
            return hmacDigest(this._id, encoding);
        }
    }

    // Export crypto namespace
    globalThis.crypto = globalThis.crypto || {};

    // randomBytes(size, callback?)
    globalThis.crypto.randomBytes = function(size, callback) {
        const result = randomBytes(size);
        if (callback) {
            // Async version - call immediately (already sync in Rust)
            setImmediate(() => callback(null, result));
            return;
        }
        return result;
    };

    // randomUUID()
    globalThis.crypto.randomUUID = function() {
        return randomUUID();
    };

    // getRandomValues(typedArray) - Web Crypto API
    globalThis.crypto.getRandomValues = function(typedArray) {
        const bytes = getRandomValues(typedArray.length);
        for (let i = 0; i < bytes.length; i++) {
            typedArray[i] = bytes[i];
        }
        return typedArray;
    };

    // createHash(algorithm)
    globalThis.crypto.createHash = function(algorithm) {
        const id = createHash(algorithm);
        return new Hash(id);
    };

    // createHmac(algorithm, key)
    globalThis.crypto.createHmac = function(algorithm, key) {
        const id = createHmac(algorithm, key);
        return new Hmac(id);
    };

    // hash(algorithm, data, encoding) - one-shot convenience
    globalThis.crypto.hash = function(algorithm, data, encoding) {
        return hash(algorithm, data, encoding);
    };

    // Aliases for compatibility
    globalThis.randomBytes = globalThis.crypto.randomBytes;
    globalThis.randomUUID = globalThis.crypto.randomUUID;

    const cryptoModule = {
        randomBytes: globalThis.crypto.randomBytes,
        randomUUID: globalThis.crypto.randomUUID,
        getRandomValues: globalThis.crypto.getRandomValues,
        createHash: globalThis.crypto.createHash,
        createHmac: globalThis.crypto.createHmac,
        hash: globalThis.crypto.hash,
    };
    cryptoModule.default = cryptoModule;

    if (globalThis.__registerModule) {
        globalThis.__registerModule('crypto', cryptoModule);
        globalThis.__registerModule('node:crypto', cryptoModule);
    }
})();
"#;

    Extension::new("crypto").with_ops(ops).with_js(js_wrapper)
}

/// Create the WebSocket extension.
///
/// Provides Web-standard WebSocket API for client-side connections.
pub fn create_websocket_extension() -> Extension {
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
        let id =
            args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
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

    // JavaScript wrapper that provides the WebSocket class
    let js_wrapper = r#"
// WebSocket wrapper - provides Web-standard WebSocket API

(function() {
    'use strict';

    const connections = new Map();

    // WebSocket class
    class WebSocket {
        static CONNECTING = 0;
        static OPEN = 1;
        static CLOSING = 2;
        static CLOSED = 3;

        constructor(url, protocols) {
            this._id = wsConnect(url);
            this._url = url;
            this._protocols = protocols || [];
            this._binaryType = 'blob';

            // Event handlers
            this.onopen = null;
            this.onmessage = null;
            this.onclose = null;
            this.onerror = null;

            // Register for event polling
            connections.set(this._id, this);
        }

        get url() {
            return this._url;
        }

        get readyState() {
            return wsReadyState(this._id);
        }

        get protocol() {
            return '';
        }

        get extensions() {
            return '';
        }

        get binaryType() {
            return this._binaryType;
        }

        set binaryType(value) {
            this._binaryType = value;
        }

        get bufferedAmount() {
            return 0;
        }

        send(data) {
            wsSend(this._id, data);
        }

        close(code, reason) {
            wsClose(this._id, code, reason);
        }

        // Internal: dispatch event
        _dispatchEvent(type, data) {
            const event = { type, target: this, ...data };

            switch (type) {
                case 'open':
                    if (this.onopen) this.onopen(event);
                    break;
                case 'message':
                    event.data = data.data;
                    if (this.onmessage) this.onmessage(event);
                    break;
                case 'close':
                    event.code = data.code;
                    event.reason = data.reason;
                    event.wasClean = data.code === 1000;
                    if (this.onclose) this.onclose(event);
                    connections.delete(this._id);
                    break;
                case 'error':
                    if (this.onerror) this.onerror(event);
                    break;
            }
        }
    }

    // Poll for WebSocket events (called from event loop)
    globalThis.__otter_ws_poll = function() {
        const events = wsPollEvents();
        for (const event of events) {
            const ws = connections.get(event.id);
            if (ws) {
                ws._dispatchEvent(event.type, event);
            }
        }
        return events.length;
    };

    // Export
    globalThis.WebSocket = WebSocket;
})();
"#;

    Extension::new("WebSocket")
        .with_ops(ops)
        .with_js(js_wrapper)
}

/// Create the Worker extension.
///
/// Provides Web Worker API for running JavaScript in background threads.
pub fn create_worker_extension() -> Extension {
    // Shared worker manager
    let manager = Arc::new(crate::worker::WorkerManager::new());

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
                crate::worker::WorkerEvent::Message(data) => json!({
                    "id": id,
                    "type": "message",
                    "data": data
                }),
                crate::worker::WorkerEvent::Error(msg) => json!({
                    "id": id,
                    "type": "error",
                    "message": msg
                }),
                crate::worker::WorkerEvent::Exit => json!({
                    "id": id,
                    "type": "exit"
                }),
                crate::worker::WorkerEvent::Terminated => json!({
                    "id": id,
                    "type": "terminated"
                }),
            })
            .collect();

        Ok(json!(json_events))
    }));

    // JavaScript wrapper that provides the Worker class
    let js_wrapper = r#"
// Worker wrapper - provides Web Worker API

(function() {
    'use strict';

    const workers = new Map();

    // Worker class
    class Worker {
        constructor(scriptURL, options) {
            // For inline workers, use URL.createObjectURL with Blob
            // For file URLs, pass the path directly
            this._script = scriptURL;
            this._id = workerCreate(scriptURL);
            this._terminated = false;

            // Event handlers
            this.onmessage = null;
            this.onerror = null;
            this.onmessageerror = null;

            // Register for event polling
            workers.set(this._id, this);
        }

        postMessage(message, transfer) {
            if (this._terminated) {
                throw new Error('Worker has been terminated');
            }
            workerPostMessage(this._id, message);
        }

        terminate() {
            if (!this._terminated) {
                this._terminated = true;
                workerTerminate(this._id);
                workers.delete(this._id);
            }
        }

        // Internal: dispatch event
        _dispatchEvent(type, data) {
            const event = { type, target: this, ...data };

            switch (type) {
                case 'message':
                    event.data = data.data;
                    if (this.onmessage) this.onmessage(event);
                    break;
                case 'error':
                    event.message = data.message;
                    if (this.onerror) this.onerror(event);
                    break;
                case 'exit':
                case 'terminated':
                    workers.delete(this._id);
                    break;
            }
        }
    }

    // Poll for Worker events (called from event loop)
    globalThis.__otter_worker_poll = function() {
        const events = workerPollEvents();
        for (const event of events) {
            const worker = workers.get(event.id);
            if (worker) {
                worker._dispatchEvent(event.type, event);
            }
        }
        return events.length;
    };

    // Export
    globalThis.Worker = Worker;
})();
"#;

    Extension::new("Worker").with_ops(ops).with_js(js_wrapper)
}

/// Create the Streams extension.
///
/// Provides Web Streams API for handling streaming data.
pub fn create_streams_extension() -> Extension {
    use crate::stream::{StreamChunk, StreamManager, StreamState};

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

    // JavaScript wrapper that provides ReadableStream and WritableStream classes
    let js_wrapper = r#"
// Web Streams API wrapper

(function() {
    'use strict';

    // ReadableStreamDefaultReader
    class ReadableStreamDefaultReader {
        constructor(stream) {
            if (stream._locked) {
                throw new TypeError('ReadableStream is locked');
            }
            this._stream = stream;
            this._streamId = stream._id;
            stream._locked = true;
            readableLock(this._streamId);
        }

        async read() {
            return readableRead(this._streamId);
        }

        releaseLock() {
            if (this._stream) {
                this._stream._locked = false;
                readableUnlock(this._streamId);
                this._stream = null;
            }
        }

        cancel(reason) {
            readableError(this._streamId, reason || 'Cancelled');
            return Promise.resolve();
        }

        get closed() {
            return Promise.resolve(); // Simplified
        }
    }

    // ReadableStreamDefaultController
    class ReadableStreamDefaultController {
        constructor(streamId) {
            this._streamId = streamId;
            this._closeRequested = false;
        }

        enqueue(chunk) {
            if (this._closeRequested) {
                throw new TypeError('Cannot enqueue after close');
            }
            readableEnqueue(this._streamId, chunk);
        }

        close() {
            if (this._closeRequested) {
                throw new TypeError('Cannot close twice');
            }
            this._closeRequested = true;
            readableClose(this._streamId);
        }

        error(e) {
            readableError(this._streamId, e ? e.message || String(e) : 'Error');
        }

        get desiredSize() {
            return 1; // Simplified
        }
    }

    // ReadableStream
    class ReadableStream {
        constructor(underlyingSource, strategy) {
            this._id = createReadableStream(strategy?.highWaterMark);
            this._locked = false;
            this._controller = new ReadableStreamDefaultController(this._id);

            // Call start if provided
            if (underlyingSource && underlyingSource.start) {
                underlyingSource.start(this._controller);
            }

            // Store pull and cancel callbacks
            this._pull = underlyingSource?.pull;
            this._cancel = underlyingSource?.cancel;
        }

        get locked() {
            return this._locked;
        }

        getReader(options) {
            if (options && options.mode === 'byob') {
                throw new TypeError('BYOB readers not supported');
            }
            return new ReadableStreamDefaultReader(this);
        }

        cancel(reason) {
            if (this._cancel) {
                this._cancel(reason);
            }
            readableError(this._id, reason || 'Cancelled');
            return Promise.resolve();
        }

        pipeTo(destination, options) {
            const reader = this.getReader();
            const writer = destination.getWriter();

            async function pump() {
                const { value, done } = await reader.read();
                if (done) {
                    writer.close();
                    return;
                }
                writer.write(value);
                return pump();
            }

            return pump();
        }

        pipeThrough(transform, options) {
            this.pipeTo(transform.writable, options);
            return transform.readable;
        }

        tee() {
            // Simplified tee - creates two readable streams
            const stream1 = new ReadableStream();
            const stream2 = new ReadableStream();
            // Not fully implemented
            return [stream1, stream2];
        }
    }

    // WritableStreamDefaultWriter
    class WritableStreamDefaultWriter {
        constructor(stream) {
            if (stream._locked) {
                throw new TypeError('WritableStream is locked');
            }
            this._stream = stream;
            this._streamId = stream._id;
            stream._locked = true;
            writableLock(this._streamId);
        }

        write(chunk) {
            writableWrite(this._streamId, chunk);
            return Promise.resolve();
        }

        close() {
            writableClose(this._streamId);
            return Promise.resolve();
        }

        abort(reason) {
            writableError(this._streamId, reason || 'Aborted');
            return Promise.resolve();
        }

        releaseLock() {
            if (this._stream) {
                this._stream._locked = false;
                writableUnlock(this._streamId);
                this._stream = null;
            }
        }

        get ready() {
            return Promise.resolve();
        }

        get closed() {
            return Promise.resolve();
        }

        get desiredSize() {
            return 1;
        }
    }

    // WritableStream
    class WritableStream {
        constructor(underlyingSink, strategy) {
            this._id = createWritableStream();
            this._locked = false;

            // Store callbacks
            this._write = underlyingSink?.write;
            this._close = underlyingSink?.close;
            this._abort = underlyingSink?.abort;

            // Call start if provided
            if (underlyingSink && underlyingSink.start) {
                underlyingSink.start({ error: (e) => writableError(this._id, e) });
            }
        }

        get locked() {
            return this._locked;
        }

        getWriter() {
            return new WritableStreamDefaultWriter(this);
        }

        abort(reason) {
            if (this._abort) {
                this._abort(reason);
            }
            writableError(this._id, reason || 'Aborted');
            return Promise.resolve();
        }

        close() {
            if (this._close) {
                this._close();
            }
            writableClose(this._id);
            return Promise.resolve();
        }
    }

    // TransformStream
    class TransformStream {
        constructor(transformer, writableStrategy, readableStrategy) {
            this.readable = new ReadableStream(undefined, readableStrategy);
            const readableController = this.readable._controller;

            this.writable = new WritableStream({
                write: (chunk) => {
                    if (transformer && transformer.transform) {
                        transformer.transform(chunk, {
                            enqueue: (c) => readableController.enqueue(c),
                            error: (e) => readableController.error(e),
                            terminate: () => readableController.close()
                        });
                    } else {
                        // Pass-through by default
                        readableController.enqueue(chunk);
                    }
                },
                close: () => {
                    if (transformer && transformer.flush) {
                        transformer.flush({
                            enqueue: (c) => readableController.enqueue(c),
                            error: (e) => readableController.error(e),
                            terminate: () => readableController.close()
                        });
                    }
                    readableController.close();
                }
            }, writableStrategy);

            // Call start if provided
            if (transformer && transformer.start) {
                transformer.start({
                    enqueue: (c) => readableController.enqueue(c),
                    error: (e) => readableController.error(e),
                    terminate: () => readableController.close()
                });
            }
        }
    }

    // Export
    globalThis.ReadableStream = ReadableStream;
    globalThis.WritableStream = WritableStream;
    globalThis.TransformStream = TransformStream;
    globalThis.ReadableStreamDefaultReader = ReadableStreamDefaultReader;
    globalThis.WritableStreamDefaultWriter = WritableStreamDefaultWriter;
})();
"#;

    Extension::new("Streams").with_ops(ops).with_js(js_wrapper)
}

/// Create the events extension (EventEmitter).
///
/// This module provides Node.js-compatible EventEmitter class.
pub fn create_events_extension() -> Extension {
    use crate::events;

    Extension::new("events").with_js(events::event_emitter_js())
}

/// Create the node:util extension.
///
/// Provides `util.promisify`, `util.inspect`, `util.format` (subset).
pub fn create_util_extension() -> Extension {
    Extension::new("util").with_js(util::util_module_js())
}

#[derive(Default)]
struct MemoryUsage {
    rss: u64,
    heap_total: u64,
    heap_used: u64,
    external: u64,
    array_buffers: u64,
}

#[cfg(target_os = "linux")]
fn memory_usage() -> MemoryUsage {
    let statm = std::fs::read_to_string("/proc/self/statm").ok();
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    if let Some(contents) = statm {
        let mut parts = contents.split_whitespace();
        let size = parts.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        let rss = parts.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        let rss_bytes = rss * page_size;
        let size_bytes = size * page_size;
        return MemoryUsage {
            rss: rss_bytes,
            heap_total: size_bytes,
            heap_used: rss_bytes,
            external: 0,
            array_buffers: 0,
        };
    }
    MemoryUsage::default()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn memory_usage() -> MemoryUsage {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    if result == 0 {
        let rss = usage.ru_maxrss as u64;
        return MemoryUsage {
            rss,
            heap_total: rss,
            heap_used: rss,
            external: 0,
            array_buffers: 0,
        };
    }
    MemoryUsage::default()
}

#[cfg(not(unix))]
fn memory_usage() -> MemoryUsage {
    MemoryUsage::default()
}

fn memory_usage_json(ctx: Option<RuntimeContextHandle>) -> serde_json::Value {
    let mut usage = memory_usage();
    if let Some(handle) = ctx {
        if let Some(stats) = jsc_heap_stats(handle.ctx()) {
            usage.heap_total = stats.heap_capacity;
            usage.heap_used = stats.heap_size;
            usage.external = stats.extra_memory;
            usage.array_buffers = stats.array_buffer;
        }
    }
    json!({
        "rss": usage.rss,
        "heapTotal": usage.heap_total,
        "heapUsed": usage.heap_used,
        "external": usage.external,
        "arrayBuffers": usage.array_buffers,
    })
}

/// Create the process extension.
///
/// This module exposes process-related helpers via ops.
pub fn create_process_extension() -> Extension {
    Extension::new("process").with_ops(vec![op_sync(
        "__otter_process_memory_usage",
        |ctx, _args| {
            let handle = ctx
                .state()
                .get::<RuntimeContextHandle>()
                .map(|h| *h);
            Ok(memory_usage_json(handle))
        },
    )])
}

/// Create the os extension.
///
/// This module provides Node.js-compatible operating system utilities.
pub fn create_os_extension() -> Extension {
    use crate::os;

    // Collect static OS info
    let platform = os::platform();
    let arch = os::arch();
    let os_type = os::os_type().as_str();
    let hostname = os::hostname();
    let homedir = os::homedir();
    let tmpdir = os::tmpdir();
    let endianness = os::endianness();
    let release = os::release();
    let version = os::version();
    let totalmem = os::totalmem();
    let freemem = os::freemem();
    let uptime = os::uptime();
    let cpus = os::cpus();
    let loadavg = os::loadavg();
    let userinfo = os::userinfo();
    let machine = os::machine();
    let eol = os::eol();

    // Create JSON for cpus
    let cpus_json = serde_json::to_string(&cpus).unwrap_or_else(|_| "[]".to_string());
    let userinfo_json = serde_json::to_string(&userinfo).unwrap_or_else(|_| "{}".to_string());

    let devnull = if cfg!(windows) {
        "\\\\.\\nul"
    } else {
        "/dev/null"
    };

    // Setup code to inject OS values
    let setup_js = format!(
        r#"
globalThis.__os_platform = {platform:?};
globalThis.__os_arch = {arch:?};
globalThis.__os_type = {os_type:?};
globalThis.__os_hostname = {hostname:?};
globalThis.__os_homedir = {homedir:?};
globalThis.__os_tmpdir = {tmpdir:?};
globalThis.__os_endianness = {endianness:?};
globalThis.__os_release = {release:?};
globalThis.__os_version = {version:?};
globalThis.__os_totalmem = {totalmem};
globalThis.__os_freemem = {freemem};
globalThis.__os_uptime = {uptime};
globalThis.__os_cpus = {cpus_json};
globalThis.__os_loadavg = [{loadavg_0}, {loadavg_1}, {loadavg_2}];
globalThis.__os_userinfo = {userinfo_json};
globalThis.__os_machine = {machine:?};
globalThis.__os_eol = {eol:?};
globalThis.__os_devnull = {devnull:?};
"#,
        platform = platform,
        arch = arch,
        os_type = os_type,
        hostname = hostname,
        homedir = homedir,
        tmpdir = tmpdir,
        endianness = endianness,
        release = release,
        version = version,
        totalmem = totalmem,
        freemem = freemem,
        uptime = uptime,
        cpus_json = cpus_json,
        loadavg_0 = loadavg[0],
        loadavg_1 = loadavg[1],
        loadavg_2 = loadavg[2],
        userinfo_json = userinfo_json,
        machine = machine,
        eol = eol,
        devnull = devnull,
    );

    // Combine setup with module code
    let full_js = format!("{}\n{}", setup_js, os::os_module_js());

    Extension::new("os").with_js(&full_js)
}

/// Create the node:child_process extension.
///
/// This module provides process spawning and IPC capabilities.
pub fn create_child_process_extension() -> Extension {
    use crate::child_process::ChildProcessManager;

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
        let cmd_arr = args
            .first()
            .and_then(|v| v.as_array())
            .ok_or_else(|| otter_runtime::error::JscError::internal("cpSpawn requires command array"))?;

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
        let cmd_arr = args
            .first()
            .and_then(|v| v.as_array())
            .ok_or_else(|| otter_runtime::error::JscError::internal("cpSpawnSync requires command array"))?;

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
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("cpWriteStdin requires id"))? as u32;

        let data = extract_buffer_data(args.get(1))?;

        mgr_write
            .write_stdin(id, data)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(null))
    }));

    // cpCloseStdin(id: number) -> null
    ops.push(op_sync("cpCloseStdin", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("cpCloseStdin requires id"))? as u32;

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
            .ok_or_else(|| otter_runtime::error::JscError::internal("cpKill requires id"))? as u32;

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
            .ok_or_else(|| otter_runtime::error::JscError::internal("cpPid requires id"))? as u32;

        Ok(json!(mgr_pid.pid(id)))
    }));

    // cpExitCode(id: number) -> number | null
    ops.push(op_sync("cpExitCode", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("cpExitCode requires id"))? as u32;

        Ok(json!(mgr_exit.exit_code(id)))
    }));

    // cpSignalCode(id: number) -> string | null
    ops.push(op_sync("cpSignalCode", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("cpSignalCode requires id"))? as u32;

        Ok(json!(mgr_signal.signal_code(id)))
    }));

    // cpIsRunning(id: number) -> boolean
    ops.push(op_sync("cpIsRunning", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("cpIsRunning requires id"))? as u32;

        Ok(json!(mgr_running.is_running(id)))
    }));

    // cpIsKilled(id: number) -> boolean
    ops.push(op_sync("cpIsKilled", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("cpIsKilled requires id"))? as u32;

        Ok(json!(mgr_killed.is_killed(id)))
    }));

    // cpRef(id: number) -> null
    ops.push(op_sync("cpRef", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("cpRef requires id"))? as u32;

        mgr_ref.ref_process(id);
        Ok(json!(null))
    }));

    // cpUnref(id: number) -> null
    ops.push(op_sync("cpUnref", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("cpUnref requires id"))? as u32;

        mgr_unref.unref_process(id);
        Ok(json!(null))
    }));

    // cpPollEvents() -> array
    ops.push(op_sync("cpPollEvents", move |_ctx, _args| {
        let events = mgr_poll.poll_events();
        let json_events: Vec<serde_json::Value> = events
            .into_iter()
            .map(|(id, event)| {
                match event {
                    crate::child_process::ChildProcessEvent::Spawn => {
                        json!({"id": id, "type": "spawn"})
                    }
                    crate::child_process::ChildProcessEvent::Stdout(data) => {
                        json!({"id": id, "type": "stdout", "data": {"type": "Buffer", "data": data}})
                    }
                    crate::child_process::ChildProcessEvent::Stderr(data) => {
                        json!({"id": id, "type": "stderr", "data": {"type": "Buffer", "data": data}})
                    }
                    crate::child_process::ChildProcessEvent::Exit { code, signal } => {
                        json!({"id": id, "type": "exit", "code": code, "signal": signal})
                    }
                    crate::child_process::ChildProcessEvent::Close { code, signal } => {
                        json!({"id": id, "type": "close", "code": code, "signal": signal})
                    }
                    crate::child_process::ChildProcessEvent::Error(msg) => {
                        json!({"id": id, "type": "error", "message": msg})
                    }
                    crate::child_process::ChildProcessEvent::Message(data) => {
                        json!({"id": id, "type": "message", "data": data})
                    }
                }
            })
            .collect();

        Ok(json!(json_events))
    }));

    // JavaScript wrapper code
    let js_code = child_process_js();

    Extension::new("child_process").with_ops(ops).with_js(&js_code)
}

/// Parse spawn options from JSON value
fn parse_spawn_options(value: Option<&serde_json::Value>) -> crate::child_process::SpawnOptions {
    use crate::child_process::{SpawnOptions, StdioConfig};

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
        stdin: obj.get("stdin").map(parse_stdio).unwrap_or(StdioConfig::Pipe),
        stdout: obj.get("stdout").map(parse_stdio).unwrap_or(StdioConfig::Pipe),
        stderr: obj.get("stderr").map(parse_stdio).unwrap_or(StdioConfig::Pipe),
        shell: obj.get("shell").and_then(|v| {
            if v.as_bool() == Some(true) {
                Some("/bin/sh".to_string())
            } else {
                v.as_str().map(String::from)
            }
        }),
        timeout: obj.get("timeout").and_then(|v| v.as_u64()),
        detached: obj.get("detached").and_then(|v| v.as_bool()).unwrap_or(false),
        ipc: obj.get("ipc").and_then(|v| v.as_bool()).unwrap_or(false),
    }
}

/// Extract buffer data from JSON value
fn extract_buffer_data(value: Option<&serde_json::Value>) -> Result<Vec<u8>, otter_runtime::error::JscError> {
    let Some(v) = value else {
        return Ok(Vec::new());
    };

    // Handle Buffer object: { type: "Buffer", data: [...] }
    if let Some(obj) = v.as_object() {
        if obj.get("type").and_then(|t| t.as_str()) == Some("Buffer") {
            if let Some(data) = obj.get("data").and_then(|d| d.as_array()) {
                return Ok(data.iter().filter_map(|b| b.as_u64().map(|n| n as u8)).collect());
            }
        }
    }

    // Handle string
    if let Some(s) = v.as_str() {
        return Ok(s.as_bytes().to_vec());
    }

    // Handle array of bytes
    if let Some(arr) = v.as_array() {
        return Ok(arr.iter().filter_map(|b| b.as_u64().map(|n| n as u8)).collect());
    }

    Ok(Vec::new())
}

/// JavaScript code for child_process module
fn child_process_js() -> String {
    r#"
(function() {
    'use strict';

    const EventEmitter = globalThis.__EventEmitter || class EventEmitter {
        constructor() { this._events = new Map(); }
        on(event, listener) {
            if (!this._events.has(event)) this._events.set(event, []);
            this._events.get(event).push(listener);
            return this;
        }
        emit(event, ...args) {
            const listeners = this._events.get(event) || [];
            listeners.forEach(fn => fn(...args));
            return listeners.length > 0;
        }
        removeListener(event, listener) {
            const listeners = this._events.get(event) || [];
            const idx = listeners.indexOf(listener);
            if (idx !== -1) listeners.splice(idx, 1);
            return this;
        }
    };

    // Track all processes for polling
    const processes = new Map();

    // === Subprocess class (Otter-native) ===
    class Subprocess {
        constructor(id, options = {}) {
            this._id = id;
            this._pid = cpPid(id);
            this._exitCode = null;
            this._signalCode = null;
            this._exited = false;
            this._onExit = options.onExit;

            // Create exited promise
            this._exitedPromise = new Promise((resolve, reject) => {
                this._resolveExited = resolve;
                this._rejectExited = reject;
            });

            // stdout as ReadableStream (if piped)
            if (options.stdout !== 'ignore' && options.stdout !== 'inherit') {
                this._stdoutChunks = [];
                this.stdout = new ReadableStream({
                    start: (controller) => {
                        this._stdoutController = controller;
                    }
                });
                // Add text() helper
                this.stdout.text = async () => {
                    const reader = this.stdout.getReader();
                    const chunks = [];
                    while (true) {
                        const { done, value } = await reader.read();
                        if (done) break;
                        chunks.push(value);
                    }
                    const decoder = new TextDecoder();
                    return chunks.map(c => decoder.decode(c)).join('');
                };
            } else {
                this.stdout = null;
            }

            // stderr as ReadableStream (if piped)
            if (options.stderr !== 'ignore' && options.stderr !== 'inherit') {
                this.stderr = new ReadableStream({
                    start: (controller) => {
                        this._stderrController = controller;
                    }
                });
                this.stderr.text = async () => {
                    const reader = this.stderr.getReader();
                    const chunks = [];
                    while (true) {
                        const { done, value } = await reader.read();
                        if (done) break;
                        chunks.push(value);
                    }
                    const decoder = new TextDecoder();
                    return chunks.map(c => decoder.decode(c)).join('');
                };
            } else {
                this.stderr = null;
            }

            // stdin as WritableStream (if piped)
            if (options.stdin !== 'ignore' && options.stdin !== 'inherit') {
                const id = this._id;
                this.stdin = new WritableStream({
                    write(chunk) {
                        let data;
                        if (typeof chunk === 'string') {
                            data = new TextEncoder().encode(chunk);
                        } else {
                            data = chunk;
                        }
                        cpWriteStdin(id, Array.from(data));
                    },
                    close() {
                        cpCloseStdin(id);
                    }
                });
            } else {
                this.stdin = null;
            }

            processes.set(id, this);
        }

        get pid() { return this._pid; }
        get exited() { return this._exitedPromise; }
        get exitCode() { return this._exitCode; }
        get signalCode() { return this._signalCode; }

        kill(signal) {
            return cpKill(this._id, signal);
        }

        ref() {
            cpRef(this._id);
            return this;
        }

        unref() {
            cpUnref(this._id);
            return this;
        }

        _handleEvent(event) {
            switch (event.type) {
                case 'stdout':
                    if (this._stdoutController) {
                        const data = event.data.data || event.data;
                        this._stdoutController.enqueue(new Uint8Array(data));
                    }
                    break;
                case 'stderr':
                    if (this._stderrController) {
                        const data = event.data.data || event.data;
                        this._stderrController.enqueue(new Uint8Array(data));
                    }
                    break;
                case 'exit':
                    this._exitCode = event.code;
                    this._signalCode = event.signal;
                    break;
                case 'close':
                    this._exited = true;
                    if (this._stdoutController) {
                        try { this._stdoutController.close(); } catch {}
                    }
                    if (this._stderrController) {
                        try { this._stderrController.close(); } catch {}
                    }
                    if (this._onExit) {
                        this._onExit(this, this._exitCode, this._signalCode, null);
                    }
                    this._resolveExited(this._exitCode ?? 0);
                    processes.delete(this._id);
                    break;
                case 'error':
                    if (this._onExit) {
                        this._onExit(this, null, null, new Error(event.message));
                    }
                    this._rejectExited(new Error(event.message));
                    processes.delete(this._id);
                    break;
            }
        }
    }

    // === ChildProcess class (Node.js-style) ===
    class ChildProcess extends EventEmitter {
        constructor(id) {
            super();
            this._id = id;
            this._pid = cpPid(id);
            this._exitCode = null;
            this._signalCode = null;
            this._killed = false;

            // Create stream-like objects
            this.stdin = new ChildStdin(id);
            this.stdout = new ChildReadable(id, 'stdout');
            this.stderr = new ChildReadable(id, 'stderr');

            processes.set(id, this);
        }

        get pid() { return this._pid; }
        get exitCode() { return this._exitCode; }
        get signalCode() { return this._signalCode; }
        get killed() { return this._killed; }

        kill(signal) {
            const result = cpKill(this._id, signal);
            if (result) this._killed = true;
            return result;
        }

        ref() {
            cpRef(this._id);
            return this;
        }

        unref() {
            cpUnref(this._id);
            return this;
        }

        _handleEvent(event) {
            switch (event.type) {
                case 'spawn':
                    this.emit('spawn');
                    break;
                case 'stdout':
                    const stdoutData = event.data.data || event.data;
                    this.stdout._push(Buffer.from(stdoutData));
                    break;
                case 'stderr':
                    const stderrData = event.data.data || event.data;
                    this.stderr._push(Buffer.from(stderrData));
                    break;
                case 'exit':
                    this._exitCode = event.code;
                    this._signalCode = event.signal;
                    this.emit('exit', event.code, event.signal);
                    break;
                case 'close':
                    this.emit('close', event.code, event.signal);
                    processes.delete(this._id);
                    break;
                case 'error':
                    this.emit('error', new Error(event.message));
                    processes.delete(this._id);
                    break;
                case 'message':
                    this.emit('message', event.data);
                    break;
            }
        }

        send(message) {
            // IPC send - to be implemented
            return true;
        }
    }

    // Stdin stream for ChildProcess
    class ChildStdin extends EventEmitter {
        constructor(id) {
            super();
            this._id = id;
            this._ended = false;
        }

        write(data, encoding, callback) {
            if (this._ended) throw new Error('write after end');
            let buffer;
            if (typeof data === 'string') {
                buffer = Buffer.from(data, encoding || 'utf8');
            } else {
                buffer = data;
            }
            cpWriteStdin(this._id, Array.from(buffer));
            if (callback) queueMicrotask(callback);
            return true;
        }

        end(data, encoding, callback) {
            if (data) this.write(data, encoding);
            this._ended = true;
            cpCloseStdin(this._id);
            if (callback) queueMicrotask(callback);
        }
    }

    // Readable stream for stdout/stderr
    class ChildReadable extends EventEmitter {
        constructor(id, type) {
            super();
            this._id = id;
            this._type = type;
        }

        _push(data) {
            this.emit('data', data);
        }
    }

    // === Public API ===

    // Otter.spawn (Otter-native)
    function otterSpawn(cmd, options = {}) {
        const id = cpSpawn(cmd, options);
        return new Subprocess(id, options);
    }

    // Otter.spawnSync (Otter-native)
    function otterSpawnSync(cmd, options = {}) {
        const result = cpSpawnSync(cmd, options);
        return {
            pid: result.pid,
            stdout: result.stdout.data ? Buffer.from(result.stdout.data) : Buffer.from([]),
            stderr: result.stderr.data ? Buffer.from(result.stderr.data) : Buffer.from([]),
            status: result.status,
            signal: result.signal,
            error: result.error ? new Error(result.error) : null,
        };
    }

    // Node.js spawn
    function spawn(command, args, options) {
        if (Array.isArray(args)) {
            // spawn(command, args, options)
        } else if (args && typeof args === 'object') {
            options = args;
            args = [];
        } else {
            args = [];
            options = {};
        }

        const cmd = [command, ...(args || [])];
        const id = cpSpawn(cmd, options || {});
        return new ChildProcess(id);
    }

    // Node.js exec
    function exec(command, options, callback) {
        if (typeof options === 'function') {
            callback = options;
            options = {};
        }
        options = options || {};

        const child = spawn(command, [], {
            ...options,
            shell: options.shell !== false ? (typeof options.shell === 'string' ? options.shell : '/bin/sh') : null,
        });

        let stdout = [];
        let stderr = [];

        child.stdout.on('data', (data) => stdout.push(data));
        child.stderr.on('data', (data) => stderr.push(data));

        child.on('close', (code, signal) => {
            const stdoutBuf = Buffer.concat(stdout);
            const stderrBuf = Buffer.concat(stderr);
            if (callback) {
                const err = code !== 0
                    ? Object.assign(new Error(`Command failed: ${command}`), { code, signal })
                    : null;
                callback(err, stdoutBuf, stderrBuf);
            }
        });

        child.on('error', (err) => {
            if (callback) callback(err, '', '');
        });

        return child;
    }

    // Node.js execSync
    function execSync(command, options) {
        options = options || {};
        const shell = options.shell !== false
            ? (typeof options.shell === 'string' ? options.shell : '/bin/sh')
            : null;

        const result = cpSpawnSync([command], { ...options, shell });

        if (result.error) {
            throw new Error(result.error);
        }

        if (result.status !== 0) {
            const err = new Error(`Command failed: ${command}`);
            err.status = result.status;
            err.signal = result.signal;
            err.stderr = result.stderr.data ? Buffer.from(result.stderr.data) : Buffer.from([]);
            throw err;
        }

        const stdout = result.stdout.data ? Buffer.from(result.stdout.data) : Buffer.from([]);
        if (options.encoding === 'utf8' || options.encoding === 'utf-8') {
            return stdout.toString('utf8');
        }
        return stdout;
    }

    // Node.js spawnSync
    function spawnSync(command, args, options) {
        if (Array.isArray(args)) {
            // spawnSync(command, args, options)
        } else if (args && typeof args === 'object') {
            options = args;
            args = [];
        } else {
            args = [];
            options = {};
        }

        const cmd = [command, ...(args || [])];
        return otterSpawnSync(cmd, options || {});
    }

    // Node.js execFile
    function execFile(file, args, options, callback) {
        if (typeof args === 'function') {
            callback = args;
            args = [];
            options = {};
        } else if (typeof options === 'function') {
            callback = options;
            options = {};
        }

        const child = spawn(file, args, options);

        let stdout = [];
        let stderr = [];

        child.stdout.on('data', (data) => stdout.push(data));
        child.stderr.on('data', (data) => stderr.push(data));

        child.on('close', (code, signal) => {
            const stdoutBuf = Buffer.concat(stdout);
            const stderrBuf = Buffer.concat(stderr);
            if (callback) {
                const err = code !== 0
                    ? Object.assign(new Error(`Command failed: ${file}`), { code, signal })
                    : null;
                callback(err, stdoutBuf, stderrBuf);
            }
        });

        return child;
    }

    // Node.js execFileSync
    function execFileSync(file, args, options) {
        if (!Array.isArray(args)) {
            options = args;
            args = [];
        }

        const cmd = [file, ...(args || [])];
        const result = cpSpawnSync(cmd, options || {});

        if (result.error) {
            throw new Error(result.error);
        }

        if (result.status !== 0) {
            const err = new Error(`Command failed: ${file}`);
            err.status = result.status;
            err.signal = result.signal;
            throw err;
        }

        return result.stdout.data ? Buffer.from(result.stdout.data) : Buffer.from([]);
    }

    // Node.js fork
    function fork(modulePath, args, options) {
        if (!Array.isArray(args)) {
            options = args;
            args = [];
        }
        options = options || {};

        // fork is spawn with ipc: true and running otter
        const execPath = options.execPath || 'otter';
        const execArgs = options.execArgv || [];

        const cmd = [execPath, ...execArgs, 'run', modulePath, ...(args || [])];
        const id = cpSpawn(cmd, { ...options, ipc: true });
        return new ChildProcess(id);
    }

    // Event loop polling
    globalThis.__otter_cp_poll = function() {
        const events = cpPollEvents();
        for (const event of events) {
            const proc = processes.get(event.id);
            if (proc) {
                proc._handleEvent(event);
            }
        }
        return events.length;
    };

    // Register Otter.spawn
    if (!globalThis.Otter) globalThis.Otter = {};
    globalThis.Otter.spawn = otterSpawn;
    globalThis.Otter.spawnSync = otterSpawnSync;

    // Export child_process module
    const childProcessModule = {
        spawn,
        spawnSync,
        exec,
        execSync,
        execFile,
        execFileSync,
        fork,
        ChildProcess,
    };

    // Register module
    if (globalThis.__registerModule) {
        globalThis.__registerModule('child_process', childProcessModule);
        globalThis.__registerModule('node:child_process', childProcessModule);
    }
})();
"#.to_string()
}

/// Create process IPC extension for child processes.
///
/// This extension provides the actual IPC functionality when running as a forked child.
/// It enables `process.send()` and `process.on('message', ...)`.
///
/// # Arguments
///
/// * `ipc_channel` - The IPC channel connected to the parent process
///
/// # Example
///
/// ```no_run
/// use otter_node::ipc::IpcChannel;
/// use otter_node::create_process_ipc_extension;
///
/// // In child process, after detecting OTTER_IPC_FD
/// let fd = std::env::var("OTTER_IPC_FD").unwrap().parse().unwrap();
/// let channel = unsafe { IpcChannel::from_raw_fd(fd).unwrap() };
/// let ext = create_process_ipc_extension(channel);
/// ```
#[cfg(unix)]
pub fn create_process_ipc_extension(ipc_channel: crate::ipc::IpcChannel) -> Extension {
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
    ops.push(op_async(
        "__otter_process_ipc_send",
        move |_ctx, args| {
            let channel = channel_send.clone();
            let msg = args
                .first()
                .cloned()
                .unwrap_or(serde_json::Value::Null);

            Box::pin(async move {
                let mut ch = channel.lock().await;
                match ch.send(&msg).await {
                    Ok(_) => Ok(json!(true)),
                    Err(_) => Ok(json!(false)),
                }
            })
        },
    ));

    // __otter_process_ipc_recv() -> message | null
    ops.push(op_async(
        "__otter_process_ipc_recv",
        move |_ctx, _args| {
            let channel = channel_recv.clone();

            Box::pin(async move {
                let mut ch = channel.lock().await;
                match ch.recv().await {
                    Ok(Some(msg)) => Ok(msg),
                    Ok(None) | Err(_) => Ok(json!(null)),
                }
            })
        },
    ));

    // __otter_process_ipc_connected() -> boolean
    ops.push(op_sync("__otter_process_ipc_connected", move |_ctx, _args| {
        Ok(json!(connected_check.load(std::sync::atomic::Ordering::Relaxed)))
    }));

    // __otter_process_ipc_disconnect() -> null
    ops.push(op_sync("__otter_process_ipc_disconnect", move |_ctx, _args| {
        connected_disconnect.store(false, std::sync::atomic::Ordering::Relaxed);
        Ok(json!(null))
    }));

    // JavaScript to set up process.send and message polling
    let js_code = r#"
(function() {
    'use strict';

    if (!globalThis.process) return;

    // Mark process as connected
    process.connected = true;

    // Override send to use IPC
    process.send = function(message, _handle, _options, callback) {
        if (typeof callback !== 'function' && typeof _handle === 'function') {
            callback = _handle;
        }
        if (typeof callback !== 'function' && typeof _options === 'function') {
            callback = _options;
        }

        return __otter_process_ipc_send(message)
            .then((result) => {
                if (callback) callback(null);
                return result;
            })
            .catch((err) => {
                if (callback) callback(err);
                return false;
            });
    };

    // Override disconnect
    process.disconnect = function() {
        __otter_process_ipc_disconnect();
        process.connected = false;
        process.emit('disconnect');
    };

    // Set up message polling
    let pollInterval = null;
    const pollMessages = async () => {
        if (!process.connected) {
            if (pollInterval) clearInterval(pollInterval);
            return;
        }

        try {
            const msg = await __otter_process_ipc_recv();
            if (msg !== null) {
                process.emit('message', msg);
            }
        } catch (e) {
            console.error('IPC poll error:', e);
        }
    };

    // Start polling when there are message listeners
    const origOn = process.on.bind(process);
    process.on = function(event, handler) {
        if (event === 'message' && !pollInterval) {
            pollInterval = setInterval(pollMessages, 10);
        }
        return origOn(event, handler);
    };
})();
"#;

    Extension::new("process_ipc")
        .with_ops(ops)
        .with_js(js_code)
}

/// Stub for non-Unix platforms
#[cfg(not(unix))]
pub fn create_process_ipc_extension(_ipc_channel: ()) -> Extension {
    Extension::new("process_ipc")
}

/// Create the URL Web API extension.
///
/// This extension provides WHATWG URL Standard compliant URL and URLSearchParams
/// classes using native Rust parsing via the `url` crate.
pub fn create_url_extension() -> Extension {
    let js_shim = url::url_module_js();

    Extension::new("url")
        .with_ops(vec![
            // Parse a URL with optional base
            op_sync("__otter_url_parse", |_ctx, args| {
                let url_string = args.first().and_then(|v| v.as_str()).unwrap_or("");
                let base = args.get(1).and_then(|v| v.as_str());

                match url::UrlComponents::parse(url_string, base) {
                    Ok(components) => Ok(serde_json::to_value(components).unwrap()),
                    Err(e) => Ok(json!({ "error": e })),
                }
            }),
            // Set a URL component
            op_sync("__otter_url_set_component", |_ctx, args| {
                let href = args.first().and_then(|v| v.as_str()).unwrap_or("");
                let component_name = args.get(1).and_then(|v| v.as_str()).unwrap_or("");
                let value = args.get(2).and_then(|v| v.as_str()).unwrap_or("");

                // Parse the current URL
                let components = match url::UrlComponents::parse(href, None) {
                    Ok(c) => c,
                    Err(e) => return Ok(json!({ "error": e })),
                };

                // Set the component
                match components.set_component(component_name, value) {
                    Ok(updated) => Ok(serde_json::to_value(updated).unwrap()),
                    Err(e) => Ok(json!({ "error": e })),
                }
            }),
        ])
        .with_js(js_shim)
}

/// Create the HTTP server extension for Otter.serve().
///
/// This extension provides a high-performance HTTP/HTTPS server with Bun-compatible API.
/// Supports HTTP/1.1 and HTTP/2 with ALPN negotiation for TLS connections.
///
/// # Arguments
///
/// * `event_tx` - Channel sender for HTTP events to the worker thread
///
/// # Returns
///
/// A tuple of (Extension, ActiveServerCount) where ActiveServerCount can be used
/// in the event loop to check if any HTTP servers are active.
pub fn create_http_server_extension(event_tx: UnboundedSender<http_server::HttpEvent>) -> (Extension, http_server::ActiveServerCount) {
    let manager = Arc::new(http_server::HttpServerManager::new());
    let active_count = manager.active_count();

    let manager_create = manager.clone();
    let manager_info = manager.clone();
    let manager_stop = manager.clone();

    let event_tx_create = event_tx.clone();

    let js_shim = include_str!("serve_shim.js");

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
                        let cert = tls_obj
                            .get("cert")
                            .and_then(|v| {
                                if let Some(s) = v.as_str() {
                                    Some(s.as_bytes().to_vec())
                                } else if let Some(arr) = v.as_array() {
                                    Some(arr.iter().filter_map(|v| v.as_u64().map(|n| n as u8)).collect())
                                } else {
                                    None
                                }
                            });

                        let key = tls_obj
                            .get("key")
                            .and_then(|v| {
                                if let Some(s) = v.as_str() {
                                    Some(s.as_bytes().to_vec())
                                } else if let Some(arr) = v.as_array() {
                                    Some(arr.iter().filter_map(|v| v.as_u64().map(|n| n as u8)).collect())
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
            op_async("__otter_http_req_body", |_ctx, args| {
                async move {
                    let request_id = args.first().and_then(|v| v.as_u64()).unwrap_or(0);
                    match http_request::read_request_body(request_id).await {
                        Some(body) => Ok(json!({ "data": body })),
                        None => Ok(json!({ "data": [] })),
                    }
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
                                        .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
                                        .unwrap_or_default()
                                }
                                _ => {
                                    // Legacy: Handle { data: [...] } format from Uint8Array
                                    obj.get("data")
                                        .and_then(|d| d.as_array())
                                        .map(|arr| arr.iter().filter_map(|v| v.as_u64().map(|n| n as u8)).collect())
                                        .unwrap_or_default()
                                }
                            }
                        } else if let Some(arr) = v.as_array() {
                            // Legacy: array of numbers
                            arr.iter().filter_map(|v| v.as_u64().map(|n| n as u8)).collect()
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
        .with_js(js_shim);

    (extension, active_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path_ext::create_path_extension;

    #[test]
    fn test_path_extension_created() {
        let _ext = create_path_extension();
        // Extension created successfully
    }

    #[test]
    fn test_buffer_extension_created() {
        let _ext = create_buffer_extension();
        // Extension created successfully
    }

    #[test]
    fn test_fs_extension_created() {
        let caps = Capabilities::none();
        let _ext = create_fs_extension(caps);
        // Extension created successfully
    }

    #[test]
    fn test_test_extension_created() {
        let _ext = create_test_extension();
        // Extension created successfully
    }

    #[test]
    fn test_crypto_extension_created() {
        let _ext = create_crypto_extension();
        // Extension created successfully
    }

    #[test]
    fn test_websocket_extension_created() {
        let _ext = create_websocket_extension();
        // Extension created successfully
    }

    #[test]
    fn test_worker_extension_created() {
        let _ext = create_worker_extension();
        // Extension created successfully
    }

    #[test]
    fn test_streams_extension_created() {
        let _ext = create_streams_extension();
        // Extension created successfully
    }

    #[test]
    fn test_events_extension_created() {
        let _ext = create_events_extension();
        // Extension created successfully
    }

    #[test]
    fn test_os_extension_created() {
        let _ext = create_os_extension();
        // Extension created successfully
    }
}
