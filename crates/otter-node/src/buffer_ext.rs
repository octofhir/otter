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
