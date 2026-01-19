//! File system extension module using the new architecture.
//!
//! This module provides the node:fs extension with both sync and async operations.
//!
//! ## Architecture
//!
//! - `fs.rs` - Rust filesystem implementation
//! - `fs_ext.rs` - Extension creation with ops
//! - `fs.js` - JavaScript wrapper
//!
//! Note: This module requires capabilities for security and doesn't fit the #[dive]
//! pattern, so we use traditional op_sync/op_async with closures.

use otter_runtime::Extension;
use otter_runtime::extension::{OpDecl, op_async, op_sync};
use serde_json::json;
use std::sync::Arc;

use crate::Capabilities;
use crate::fs;

/// Create the fs extension.
///
/// This extension provides Node.js-compatible file system operations.
/// Requires capabilities to control read/write access.
pub fn extension(capabilities: Capabilities) -> Extension {
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

    // unlinkSync (alias for rmSync for files)
    let caps_unlink_sync = caps.clone();
    ops.push(op_sync("unlinkSync", move |_ctx, args| {
        let path = args
            .first()
            .and_then(|v| v.as_str())
            .ok_or_else(|| otter_runtime::error::JscError::internal("unlinkSync requires path"))?;

        let path_buf = std::path::Path::new(path).to_path_buf();
        if !caps_unlink_sync.can_write(&path_buf) {
            return Err(otter_runtime::error::JscError::internal(format!(
                "Permission denied: write access to '{}'",
                path
            )));
        }

        std::fs::remove_file(&path_buf).map_err(|e| {
            otter_runtime::error::JscError::internal(format!("Failed to unlink '{}': {}", path, e))
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

    // unlink
    let caps_unlink = caps.clone();
    ops.push(op_async("unlink", move |_ctx, args| {
        let caps = caps_unlink.clone();
        async move {
            let path = args
                .first()
                .and_then(|v| v.as_str())
                .ok_or_else(|| otter_runtime::error::JscError::internal("unlink requires path"))?;

            fs::unlink(&caps, path)
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

    Extension::new("fs")
        .with_ops(ops)
        .with_js(include_str!("fs.js"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension(Capabilities::none());
        assert_eq!(ext.name(), "fs");
        assert!(ext.js_code().is_some());
    }
}
