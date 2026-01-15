//! JavaScript extensions for Node.js modules.
//!
//! These extensions expose the Rust implementations to JavaScript.

use crate::{buffer, fs, path};
use otter_engine::Capabilities;
use otter_runtime::extension::{Extension, OpDecl, op_async, op_sync};
use serde_json::json;
use std::sync::Arc;

/// Create the node:path extension.
///
/// This module provides path manipulation utilities compatible with Node.js.
pub fn create_path_extension() -> Extension {
    Extension::new("path").with_ops(vec![
        op_sync("join", |_ctx, args| {
            let paths: Vec<&str> = args.iter().filter_map(|v| v.as_str()).collect();
            Ok(json!(path::join(&paths)))
        }),
        op_sync("resolve", |_ctx, args| {
            let paths: Vec<&str> = args.iter().filter_map(|v| v.as_str()).collect();
            Ok(json!(path::resolve(&paths)))
        }),
        op_sync("dirname", |_ctx, args| {
            let p = args.first().and_then(|v| v.as_str()).unwrap_or("");
            Ok(json!(path::dirname(p)))
        }),
        op_sync("basename", |_ctx, args| {
            let p = args.first().and_then(|v| v.as_str()).unwrap_or("");
            let suffix = args.get(1).and_then(|v| v.as_str());
            Ok(json!(path::basename(p, suffix)))
        }),
        op_sync("extname", |_ctx, args| {
            let p = args.first().and_then(|v| v.as_str()).unwrap_or("");
            Ok(json!(path::extname(p)))
        }),
        op_sync("isAbsolute", |_ctx, args| {
            let p = args.first().and_then(|v| v.as_str()).unwrap_or("");
            Ok(json!(path::is_absolute(p)))
        }),
        op_sync("normalize", |_ctx, args| {
            let p = args.first().and_then(|v| v.as_str()).unwrap_or("");
            Ok(json!(path::normalize(p)))
        }),
        op_sync("relative", |_ctx, args| {
            let from = args.first().and_then(|v| v.as_str()).unwrap_or("");
            let to = args.get(1).and_then(|v| v.as_str()).unwrap_or("");
            Ok(json!(path::relative(from, to)))
        }),
        op_sync("parse", |_ctx, args| {
            let p = args.first().and_then(|v| v.as_str()).unwrap_or("");
            let parsed = path::parse(p);
            Ok(json!({
                "root": parsed.root,
                "dir": parsed.dir,
                "base": parsed.base,
                "ext": parsed.ext,
                "name": parsed.name,
            }))
        }),
        op_sync("format", |_ctx, args| {
            let default = json!({});
            let obj = args.first().unwrap_or(&default);
            let parsed = path::ParsedPath {
                root: obj
                    .get("root")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                dir: obj
                    .get("dir")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                base: obj
                    .get("base")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                ext: obj
                    .get("ext")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                name: obj
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            };
            Ok(json!(path::format(&parsed)))
        }),
        op_sync("sep", |_ctx, _args| Ok(json!(path::sep().to_string()))),
        op_sync("delimiter", |_ctx, _args| {
            Ok(json!(path::delimiter().to_string()))
        }),
    ])
}

/// Create the node:buffer extension.
///
/// This module provides Buffer class for binary data manipulation.
pub fn create_buffer_extension() -> Extension {
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

    Extension::new("fs").with_ops(ops)
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
})();
"#;

    Extension::new("test").with_ops(ops).with_js(js_wrapper)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
