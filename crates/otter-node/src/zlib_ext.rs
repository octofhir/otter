//! Zlib extension module.
//!
//! Provides node:zlib compatible compression/decompression APIs.
//!
//! ## Supported algorithms
//!
//! - gzip/gunzip - Gzip compression
//! - deflate/inflate - Zlib format compression
//! - deflateRaw/inflateRaw - Raw deflate compression
//! - brotliCompress/brotliDecompress - Brotli compression

use otter_runtime::Extension;
use otter_runtime::extension::op_sync;
use serde_json::json;

use crate::zlib::{self, CompressOptions, DEFAULT_CHUNK_SIZE, constants};

/// Helper to extract bytes from JS value (string, array, or Buffer object).
fn extract_bytes(value: &serde_json::Value) -> Vec<u8> {
    if let Some(s) = value.as_str() {
        s.as_bytes().to_vec()
    } else if let Some(arr) = value.as_array() {
        arr.iter()
            .filter_map(|v| v.as_u64().map(|n| n as u8))
            .collect()
    } else if let Some(obj) = value.as_object() {
        // Buffer object: { type: "Buffer", data: [...] }
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
    }
}

/// Helper to extract compression options from JS options object.
fn extract_options(value: Option<&serde_json::Value>) -> Option<CompressOptions> {
    let obj = value?.as_object()?;

    Some(CompressOptions {
        level: obj
            .get("level")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
            .unwrap_or(constants::Z_DEFAULT_COMPRESSION),
        mem_level: obj
            .get("memLevel")
            .and_then(|v| v.as_u64())
            .map(|v| v as u8)
            .unwrap_or(8),
        strategy: obj
            .get("strategy")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
            .unwrap_or(constants::Z_DEFAULT_STRATEGY),
        window_bits: obj
            .get("windowBits")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
            .unwrap_or(15),
        chunk_size: obj
            .get("chunkSize")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_CHUNK_SIZE),
        dictionary: obj
            .get("dictionary")
            .map(|value| extract_bytes(value))
            .filter(|dict| !dict.is_empty()),
    })
}

/// Create the zlib extension.
pub fn extension() -> Extension {
    Extension::new("zlib")
        .with_ops(vec![
            // Gzip
            op_sync("__otter_zlib_gzip_sync", |_ctx, args| {
                let data = args.first().ok_or_else(|| {
                    otter_runtime::error::JscError::internal("gzipSync requires data")
                })?;

                let bytes = extract_bytes(data);
                let options = extract_options(args.get(1));

                match zlib::gzip(&bytes, options) {
                    Ok(compressed) => Ok(json!({
                        "type": "Buffer",
                        "data": compressed,
                    })),
                    Err(e) => Err(otter_runtime::error::JscError::internal(e.to_string())),
                }
            }),
            op_sync("__otter_zlib_gunzip_sync", |_ctx, args| {
                let data = args.first().ok_or_else(|| {
                    otter_runtime::error::JscError::internal("gunzipSync requires data")
                })?;

                let bytes = extract_bytes(data);

                match zlib::gunzip(&bytes) {
                    Ok(decompressed) => Ok(json!({
                        "type": "Buffer",
                        "data": decompressed,
                    })),
                    Err(e) => Err(otter_runtime::error::JscError::internal(e.to_string())),
                }
            }),
            // Deflate (zlib format)
            op_sync("__otter_zlib_deflate_sync", |_ctx, args| {
                let data = args.first().ok_or_else(|| {
                    otter_runtime::error::JscError::internal("deflateSync requires data")
                })?;

                let bytes = extract_bytes(data);
                let options = extract_options(args.get(1));

                match zlib::deflate(&bytes, options) {
                    Ok(compressed) => Ok(json!({
                        "type": "Buffer",
                        "data": compressed,
                    })),
                    Err(e) => Err(otter_runtime::error::JscError::internal(e.to_string())),
                }
            }),
            op_sync("__otter_zlib_inflate_sync", |_ctx, args| {
                let data = args.first().ok_or_else(|| {
                    otter_runtime::error::JscError::internal("inflateSync requires data")
                })?;

                let bytes = extract_bytes(data);
                let options = extract_options(args.get(1));

                match zlib::inflate(&bytes, options) {
                    Ok(decompressed) => Ok(json!({
                        "type": "Buffer",
                        "data": decompressed,
                    })),
                    Err(e) => Err(otter_runtime::error::JscError::internal(e.to_string())),
                }
            }),
            // Deflate Raw
            op_sync("__otter_zlib_deflate_raw_sync", |_ctx, args| {
                let data = args.first().ok_or_else(|| {
                    otter_runtime::error::JscError::internal("deflateRawSync requires data")
                })?;

                let bytes = extract_bytes(data);
                let options = extract_options(args.get(1));

                match zlib::deflate_raw(&bytes, options) {
                    Ok(compressed) => Ok(json!({
                        "type": "Buffer",
                        "data": compressed,
                    })),
                    Err(e) => Err(otter_runtime::error::JscError::internal(e.to_string())),
                }
            }),
            op_sync("__otter_zlib_inflate_raw_sync", |_ctx, args| {
                let data = args.first().ok_or_else(|| {
                    otter_runtime::error::JscError::internal("inflateRawSync requires data")
                })?;

                let bytes = extract_bytes(data);
                let options = extract_options(args.get(1));

                match zlib::inflate_raw(&bytes, options) {
                    Ok(decompressed) => Ok(json!({
                        "type": "Buffer",
                        "data": decompressed,
                    })),
                    Err(e) => Err(otter_runtime::error::JscError::internal(e.to_string())),
                }
            }),
            // Brotli
            op_sync("__otter_zlib_brotli_compress_sync", |_ctx, args| {
                let data = args.first().ok_or_else(|| {
                    otter_runtime::error::JscError::internal("brotliCompressSync requires data")
                })?;

                let bytes = extract_bytes(data);
                let quality = args
                    .get(1)
                    .and_then(|v| v.as_object())
                    .and_then(|obj| obj.get("params"))
                    .and_then(|v| v.as_object())
                    .and_then(|p| p.get("BROTLI_PARAM_QUALITY"))
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);

                match zlib::brotli_compress(&bytes, quality) {
                    Ok(compressed) => Ok(json!({
                        "type": "Buffer",
                        "data": compressed,
                    })),
                    Err(e) => Err(otter_runtime::error::JscError::internal(e.to_string())),
                }
            }),
            op_sync("__otter_zlib_brotli_decompress_sync", |_ctx, args| {
                let data = args.first().ok_or_else(|| {
                    otter_runtime::error::JscError::internal("brotliDecompressSync requires data")
                })?;

                let bytes = extract_bytes(data);

                match zlib::brotli_decompress(&bytes) {
                    Ok(decompressed) => Ok(json!({
                        "type": "Buffer",
                        "data": decompressed,
                    })),
                    Err(e) => Err(otter_runtime::error::JscError::internal(e.to_string())),
                }
            }),
            // CRC32
            op_sync("__otter_zlib_crc32", |_ctx, args| {
                let data = args.first().ok_or_else(|| {
                    otter_runtime::error::JscError::internal("crc32 requires data")
                })?;

                let bytes = extract_bytes(data);
                let checksum = zlib::crc32(&bytes);

                Ok(json!(checksum))
            }),
        ])
        .with_js(include_str!("zlib.js"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension();
        assert_eq!(ext.name(), "zlib");
        assert!(ext.js_code().is_some());
    }

    #[test]
    fn test_extract_bytes_string() {
        let value = json!("hello");
        assert_eq!(extract_bytes(&value), b"hello");
    }

    #[test]
    fn test_extract_bytes_array() {
        let value = json!([104, 101, 108, 108, 111]);
        assert_eq!(extract_bytes(&value), b"hello");
    }

    #[test]
    fn test_extract_bytes_buffer() {
        let value = json!({
            "type": "Buffer",
            "data": [104, 101, 108, 108, 111]
        });
        assert_eq!(extract_bytes(&value), b"hello");
    }

    #[test]
    fn test_extract_options() {
        let value = json!({
            "level": 9,
            "memLevel": 9,
            "strategy": 0,
            "windowBits": 12,
            "chunkSize": 8192,
            "dictionary": "abc"
        });

        let opts = extract_options(Some(&value)).unwrap();
        assert_eq!(opts.level, 9);
        assert_eq!(opts.mem_level, 9);
        assert_eq!(opts.strategy, 0);

        assert_eq!(opts.window_bits, 12);
        assert_eq!(opts.chunk_size, 8192);
        assert_eq!(opts.dictionary.as_deref(), Some(b"abc".as_slice()));
    }
}
