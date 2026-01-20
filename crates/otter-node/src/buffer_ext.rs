//! Buffer extension module using the new architecture.
//!
//! This module provides the node:buffer extension for binary data manipulation.
//!
//! ## Architecture
//!
//! - `buffer.rs` - Rust Buffer implementation
//! - `buffer_ext.rs` - Extension creation with ops
//! - `buffer.js` - JavaScript Buffer class wrapper

use otter_runtime::Extension;
use otter_runtime::extension::op_sync;
use serde_json::json;

use crate::buffer;

/// Create the buffer extension.
///
/// This extension provides Node.js-compatible Buffer class for binary data manipulation.
pub fn extension() -> Extension {
    Extension::new("Buffer")
        .with_ops(vec![
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
                let buffer_obj = args.first().ok_or_else(|| {
                    otter_runtime::error::JscError::internal("slice requires Buffer")
                })?;

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
            op_sync("copy", |_ctx, args| {
                let source_data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();

                let mut target_data: Vec<u8> = args
                    .get(1)
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();

                let target_start = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let source_start = args.get(3).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let source_end = args
                    .get(4)
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize)
                    .unwrap_or(source_data.len());

                let source = buffer::Buffer::from_bytes(&source_data);
                let mut target = buffer::Buffer::from_bytes(&target_data);
                let copied = source.copy_to(&mut target, target_start, source_start, source_end);

                Ok(json!({
                    "copied": copied,
                    "targetData": target.as_bytes(),
                }))
            }),
            op_sync("fill", |_ctx, args| {
                let mut data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();

                let len = data.len();
                let mut buf = buffer::Buffer::from_bytes(&data);

                let value = args.get(1);
                let offset = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let end = args
                    .get(3)
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize)
                    .unwrap_or(len);
                let encoding = args.get(4).and_then(|v| v.as_str()).unwrap_or("utf8");

                if let Some(v) = value {
                    if let Some(n) = v.as_u64() {
                        buf.fill(n as u8, offset, end);
                    } else if let Some(s) = v.as_str() {
                        buf.fill_string(s, encoding, offset, end);
                    }
                }

                Ok(json!({
                    "type": "Buffer",
                    "data": buf.as_bytes(),
                }))
            }),
            op_sync("indexOf", |_ctx, args| {
                let buf_data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();

                let value = args.get(1);
                let byte_offset = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let encoding = args.get(3).and_then(|v| v.as_str()).unwrap_or("utf8");

                let search_bytes: Vec<u8> = if let Some(v) = value {
                    if let Some(n) = v.as_u64() {
                        vec![n as u8]
                    } else if let Some(s) = v.as_str() {
                        match encoding {
                            "base64" => base64::Engine::decode(
                                &base64::engine::general_purpose::STANDARD,
                                s,
                            )
                            .unwrap_or_default(),
                            "hex" => hex::decode(s).unwrap_or_default(),
                            _ => s.as_bytes().to_vec(),
                        }
                    } else if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
                        arr.iter()
                            .filter_map(|x| x.as_u64().map(|n| n as u8))
                            .collect()
                    } else {
                        vec![]
                    }
                } else {
                    vec![]
                };

                let buf = buffer::Buffer::from_bytes(&buf_data);
                match buf.index_of(&search_bytes, byte_offset) {
                    Some(idx) => Ok(json!(idx)),
                    None => Ok(json!(-1)),
                }
            }),
            op_sync("lastIndexOf", |_ctx, args| {
                let buf_data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();

                let value = args.get(1);
                let byte_offset = args
                    .get(2)
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize)
                    .unwrap_or(buf_data.len());
                let encoding = args.get(3).and_then(|v| v.as_str()).unwrap_or("utf8");

                let search_bytes: Vec<u8> = if let Some(v) = value {
                    if let Some(n) = v.as_u64() {
                        vec![n as u8]
                    } else if let Some(s) = v.as_str() {
                        match encoding {
                            "base64" => base64::Engine::decode(
                                &base64::engine::general_purpose::STANDARD,
                                s,
                            )
                            .unwrap_or_default(),
                            "hex" => hex::decode(s).unwrap_or_default(),
                            _ => s.as_bytes().to_vec(),
                        }
                    } else if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
                        arr.iter()
                            .filter_map(|x| x.as_u64().map(|n| n as u8))
                            .collect()
                    } else {
                        vec![]
                    }
                } else {
                    vec![]
                };

                let buf = buffer::Buffer::from_bytes(&buf_data);
                match buf.last_index_of(&search_bytes, byte_offset) {
                    Some(idx) => Ok(json!(idx)),
                    None => Ok(json!(-1)),
                }
            }),
            op_sync("includes", |_ctx, args| {
                let buf_data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();

                let value = args.get(1);
                let byte_offset = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let encoding = args.get(3).and_then(|v| v.as_str()).unwrap_or("utf8");

                let search_bytes: Vec<u8> = if let Some(v) = value {
                    if let Some(n) = v.as_u64() {
                        vec![n as u8]
                    } else if let Some(s) = v.as_str() {
                        match encoding {
                            "base64" => base64::Engine::decode(
                                &base64::engine::general_purpose::STANDARD,
                                s,
                            )
                            .unwrap_or_default(),
                            "hex" => hex::decode(s).unwrap_or_default(),
                            _ => s.as_bytes().to_vec(),
                        }
                    } else if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
                        arr.iter()
                            .filter_map(|x| x.as_u64().map(|n| n as u8))
                            .collect()
                    } else {
                        vec![]
                    }
                } else {
                    vec![]
                };

                let buf = buffer::Buffer::from_bytes(&buf_data);
                Ok(json!(buf.includes(&search_bytes, byte_offset)))
            }),
            op_sync("write", |_ctx, args| {
                let mut data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();

                let string = args.get(1).and_then(|v| v.as_str()).unwrap_or("");
                let offset = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let length = args
                    .get(3)
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize)
                    .unwrap_or(data.len().saturating_sub(offset));
                let encoding = args.get(4).and_then(|v| v.as_str()).unwrap_or("utf8");

                let mut buf = buffer::Buffer::from_bytes(&data);
                let written = buf.write(string, offset, length, encoding);

                Ok(json!({
                    "written": written,
                    "data": buf.as_bytes(),
                }))
            }),
            op_sync("swap16", |_ctx, args| {
                let mut data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();

                let mut buf = buffer::Buffer::from_bytes(&data);
                buf.swap16()
                    .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

                Ok(json!({
                    "type": "Buffer",
                    "data": buf.as_bytes(),
                }))
            }),
            op_sync("swap32", |_ctx, args| {
                let mut data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();

                let mut buf = buffer::Buffer::from_bytes(&data);
                buf.swap32()
                    .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

                Ok(json!({
                    "type": "Buffer",
                    "data": buf.as_bytes(),
                }))
            }),
            op_sync("swap64", |_ctx, args| {
                let mut data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();

                let mut buf = buffer::Buffer::from_bytes(&data);
                buf.swap64()
                    .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

                Ok(json!({
                    "type": "Buffer",
                    "data": buf.as_bytes(),
                }))
            }),
            // Read methods
            op_sync("readUInt8", |_ctx, args| {
                let data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let offset = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let buf = buffer::Buffer::from_bytes(&data);
                match buf.read_uint8(offset) {
                    Some(v) => Ok(json!(v)),
                    None => Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE")),
                }
            }),
            op_sync("readInt8", |_ctx, args| {
                let data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let offset = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let buf = buffer::Buffer::from_bytes(&data);
                match buf.read_int8(offset) {
                    Some(v) => Ok(json!(v)),
                    None => Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE")),
                }
            }),
            op_sync("readUInt16LE", |_ctx, args| {
                let data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let offset = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let buf = buffer::Buffer::from_bytes(&data);
                match buf.read_uint16_le(offset) {
                    Some(v) => Ok(json!(v)),
                    None => Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE")),
                }
            }),
            op_sync("readUInt16BE", |_ctx, args| {
                let data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let offset = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let buf = buffer::Buffer::from_bytes(&data);
                match buf.read_uint16_be(offset) {
                    Some(v) => Ok(json!(v)),
                    None => Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE")),
                }
            }),
            op_sync("readInt16LE", |_ctx, args| {
                let data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let offset = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let buf = buffer::Buffer::from_bytes(&data);
                match buf.read_int16_le(offset) {
                    Some(v) => Ok(json!(v)),
                    None => Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE")),
                }
            }),
            op_sync("readInt16BE", |_ctx, args| {
                let data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let offset = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let buf = buffer::Buffer::from_bytes(&data);
                match buf.read_int16_be(offset) {
                    Some(v) => Ok(json!(v)),
                    None => Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE")),
                }
            }),
            op_sync("readUInt32LE", |_ctx, args| {
                let data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let offset = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let buf = buffer::Buffer::from_bytes(&data);
                match buf.read_uint32_le(offset) {
                    Some(v) => Ok(json!(v)),
                    None => Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE")),
                }
            }),
            op_sync("readUInt32BE", |_ctx, args| {
                let data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let offset = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let buf = buffer::Buffer::from_bytes(&data);
                match buf.read_uint32_be(offset) {
                    Some(v) => Ok(json!(v)),
                    None => Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE")),
                }
            }),
            op_sync("readInt32LE", |_ctx, args| {
                let data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let offset = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let buf = buffer::Buffer::from_bytes(&data);
                match buf.read_int32_le(offset) {
                    Some(v) => Ok(json!(v)),
                    None => Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE")),
                }
            }),
            op_sync("readInt32BE", |_ctx, args| {
                let data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let offset = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let buf = buffer::Buffer::from_bytes(&data);
                match buf.read_int32_be(offset) {
                    Some(v) => Ok(json!(v)),
                    None => Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE")),
                }
            }),
            op_sync("readFloatLE", |_ctx, args| {
                let data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let offset = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let buf = buffer::Buffer::from_bytes(&data);
                match buf.read_float_le(offset) {
                    Some(v) => Ok(json!(v)),
                    None => Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE")),
                }
            }),
            op_sync("readFloatBE", |_ctx, args| {
                let data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let offset = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let buf = buffer::Buffer::from_bytes(&data);
                match buf.read_float_be(offset) {
                    Some(v) => Ok(json!(v)),
                    None => Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE")),
                }
            }),
            op_sync("readDoubleLE", |_ctx, args| {
                let data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let offset = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let buf = buffer::Buffer::from_bytes(&data);
                match buf.read_double_le(offset) {
                    Some(v) => Ok(json!(v)),
                    None => Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE")),
                }
            }),
            op_sync("readDoubleBE", |_ctx, args| {
                let data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let offset = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let buf = buffer::Buffer::from_bytes(&data);
                match buf.read_double_be(offset) {
                    Some(v) => Ok(json!(v)),
                    None => Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE")),
                }
            }),
            op_sync("readBigInt64LE", |_ctx, args| {
                let data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let offset = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let buf = buffer::Buffer::from_bytes(&data);
                match buf.read_big_int64_le(offset) {
                    Some(v) => Ok(json!(v.to_string())),
                    None => Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE")),
                }
            }),
            op_sync("readBigInt64BE", |_ctx, args| {
                let data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let offset = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let buf = buffer::Buffer::from_bytes(&data);
                match buf.read_big_int64_be(offset) {
                    Some(v) => Ok(json!(v.to_string())),
                    None => Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE")),
                }
            }),
            op_sync("readBigUInt64LE", |_ctx, args| {
                let data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let offset = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let buf = buffer::Buffer::from_bytes(&data);
                match buf.read_big_uint64_le(offset) {
                    Some(v) => Ok(json!(v.to_string())),
                    None => Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE")),
                }
            }),
            op_sync("readBigUInt64BE", |_ctx, args| {
                let data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let offset = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let buf = buffer::Buffer::from_bytes(&data);
                match buf.read_big_uint64_be(offset) {
                    Some(v) => Ok(json!(v.to_string())),
                    None => Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE")),
                }
            }),
            // Write methods
            op_sync("writeUInt8", |_ctx, args| {
                let mut data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let value = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as u8;
                let offset = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let mut buf = buffer::Buffer::from_bytes(&data);
                if buf.write_uint8(value, offset) {
                    Ok(json!({ "type": "Buffer", "data": buf.as_bytes(), "offset": offset + 1 }))
                } else {
                    Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE"))
                }
            }),
            op_sync("writeInt8", |_ctx, args| {
                let mut data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let value = args.get(1).and_then(|v| v.as_i64()).unwrap_or(0) as i8;
                let offset = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let mut buf = buffer::Buffer::from_bytes(&data);
                if buf.write_int8(value, offset) {
                    Ok(json!({ "type": "Buffer", "data": buf.as_bytes(), "offset": offset + 1 }))
                } else {
                    Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE"))
                }
            }),
            op_sync("writeUInt16LE", |_ctx, args| {
                let mut data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let value = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as u16;
                let offset = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let mut buf = buffer::Buffer::from_bytes(&data);
                if buf.write_uint16_le(value, offset) {
                    Ok(json!({ "type": "Buffer", "data": buf.as_bytes(), "offset": offset + 2 }))
                } else {
                    Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE"))
                }
            }),
            op_sync("writeUInt16BE", |_ctx, args| {
                let mut data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let value = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as u16;
                let offset = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let mut buf = buffer::Buffer::from_bytes(&data);
                if buf.write_uint16_be(value, offset) {
                    Ok(json!({ "type": "Buffer", "data": buf.as_bytes(), "offset": offset + 2 }))
                } else {
                    Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE"))
                }
            }),
            op_sync("writeInt16LE", |_ctx, args| {
                let mut data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let value = args.get(1).and_then(|v| v.as_i64()).unwrap_or(0) as i16;
                let offset = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let mut buf = buffer::Buffer::from_bytes(&data);
                if buf.write_int16_le(value, offset) {
                    Ok(json!({ "type": "Buffer", "data": buf.as_bytes(), "offset": offset + 2 }))
                } else {
                    Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE"))
                }
            }),
            op_sync("writeInt16BE", |_ctx, args| {
                let mut data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let value = args.get(1).and_then(|v| v.as_i64()).unwrap_or(0) as i16;
                let offset = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let mut buf = buffer::Buffer::from_bytes(&data);
                if buf.write_int16_be(value, offset) {
                    Ok(json!({ "type": "Buffer", "data": buf.as_bytes(), "offset": offset + 2 }))
                } else {
                    Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE"))
                }
            }),
            op_sync("writeUInt32LE", |_ctx, args| {
                let mut data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let value = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let offset = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let mut buf = buffer::Buffer::from_bytes(&data);
                if buf.write_uint32_le(value, offset) {
                    Ok(json!({ "type": "Buffer", "data": buf.as_bytes(), "offset": offset + 4 }))
                } else {
                    Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE"))
                }
            }),
            op_sync("writeUInt32BE", |_ctx, args| {
                let mut data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let value = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let offset = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let mut buf = buffer::Buffer::from_bytes(&data);
                if buf.write_uint32_be(value, offset) {
                    Ok(json!({ "type": "Buffer", "data": buf.as_bytes(), "offset": offset + 4 }))
                } else {
                    Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE"))
                }
            }),
            op_sync("writeInt32LE", |_ctx, args| {
                let mut data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let value = args.get(1).and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                let offset = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let mut buf = buffer::Buffer::from_bytes(&data);
                if buf.write_int32_le(value, offset) {
                    Ok(json!({ "type": "Buffer", "data": buf.as_bytes(), "offset": offset + 4 }))
                } else {
                    Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE"))
                }
            }),
            op_sync("writeInt32BE", |_ctx, args| {
                let mut data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let value = args.get(1).and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                let offset = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let mut buf = buffer::Buffer::from_bytes(&data);
                if buf.write_int32_be(value, offset) {
                    Ok(json!({ "type": "Buffer", "data": buf.as_bytes(), "offset": offset + 4 }))
                } else {
                    Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE"))
                }
            }),
            op_sync("writeFloatLE", |_ctx, args| {
                let mut data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let value = args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                let offset = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let mut buf = buffer::Buffer::from_bytes(&data);
                if buf.write_float_le(value, offset) {
                    Ok(json!({ "type": "Buffer", "data": buf.as_bytes(), "offset": offset + 4 }))
                } else {
                    Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE"))
                }
            }),
            op_sync("writeFloatBE", |_ctx, args| {
                let mut data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let value = args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                let offset = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let mut buf = buffer::Buffer::from_bytes(&data);
                if buf.write_float_be(value, offset) {
                    Ok(json!({ "type": "Buffer", "data": buf.as_bytes(), "offset": offset + 4 }))
                } else {
                    Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE"))
                }
            }),
            op_sync("writeDoubleLE", |_ctx, args| {
                let mut data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let value = args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
                let offset = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let mut buf = buffer::Buffer::from_bytes(&data);
                if buf.write_double_le(value, offset) {
                    Ok(json!({ "type": "Buffer", "data": buf.as_bytes(), "offset": offset + 8 }))
                } else {
                    Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE"))
                }
            }),
            op_sync("writeDoubleBE", |_ctx, args| {
                let mut data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let value = args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
                let offset = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let mut buf = buffer::Buffer::from_bytes(&data);
                if buf.write_double_be(value, offset) {
                    Ok(json!({ "type": "Buffer", "data": buf.as_bytes(), "offset": offset + 8 }))
                } else {
                    Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE"))
                }
            }),
            op_sync("writeBigInt64LE", |_ctx, args| {
                let data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let value = args
                    .get(1)
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0);
                let offset = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let mut buf = buffer::Buffer::from_bytes(&data);
                if buf.write_big_int64_le(value, offset) {
                    Ok(json!({ "type": "Buffer", "data": buf.as_bytes(), "offset": offset + 8 }))
                } else {
                    Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE"))
                }
            }),
            op_sync("writeBigInt64BE", |_ctx, args| {
                let data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let value = args
                    .get(1)
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0);
                let offset = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let mut buf = buffer::Buffer::from_bytes(&data);
                if buf.write_big_int64_be(value, offset) {
                    Ok(json!({ "type": "Buffer", "data": buf.as_bytes(), "offset": offset + 8 }))
                } else {
                    Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE"))
                }
            }),
            op_sync("writeBigUInt64LE", |_ctx, args| {
                let data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let value = args
                    .get(1)
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                let offset = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let mut buf = buffer::Buffer::from_bytes(&data);
                if buf.write_big_uint64_le(value, offset) {
                    Ok(json!({ "type": "Buffer", "data": buf.as_bytes(), "offset": offset + 8 }))
                } else {
                    Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE"))
                }
            }),
            op_sync("writeBigUInt64BE", |_ctx, args| {
                let data: Vec<u8> = args
                    .first()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect()
                    })
                    .unwrap_or_default();
                let value = args
                    .get(1)
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                let offset = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let mut buf = buffer::Buffer::from_bytes(&data);
                if buf.write_big_uint64_be(value, offset) {
                    Ok(json!({ "type": "Buffer", "data": buf.as_bytes(), "offset": offset + 8 }))
                } else {
                    Err(otter_runtime::error::JscError::internal("ERR_OUT_OF_RANGE"))
                }
            }),
        ])
        .with_js(include_str!("buffer.js"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension();
        assert_eq!(ext.name(), "Buffer");
        assert!(ext.js_code().is_some());
    }
}
