//! Node.js zlib module implementation.
//!
//! Provides compression/decompression using gzip, deflate, and brotli algorithms.
//! Uses flate2 (with zlib-rs backend) for gzip/deflate and brotli crate for brotli.

use std::io::{Read, Write};
use thiserror::Error;

/// Compression level constants matching Node.js zlib.
pub mod constants {
    pub const Z_NO_COMPRESSION: i32 = 0;
    pub const Z_BEST_SPEED: i32 = 1;
    pub const Z_BEST_COMPRESSION: i32 = 9;
    pub const Z_DEFAULT_COMPRESSION: i32 = -1;

    // Brotli quality levels
    pub const BROTLI_MIN_QUALITY: u32 = 0;
    pub const BROTLI_MAX_QUALITY: u32 = 11;
    pub const BROTLI_DEFAULT_QUALITY: u32 = 11;

    // Flush modes
    pub const Z_NO_FLUSH: i32 = 0;
    pub const Z_PARTIAL_FLUSH: i32 = 1;
    pub const Z_SYNC_FLUSH: i32 = 2;
    pub const Z_FULL_FLUSH: i32 = 3;
    pub const Z_FINISH: i32 = 4;
    pub const Z_BLOCK: i32 = 5;
    pub const Z_TREES: i32 = 6;

    // Strategy
    pub const Z_FILTERED: i32 = 1;
    pub const Z_HUFFMAN_ONLY: i32 = 2;
    pub const Z_RLE: i32 = 3;
    pub const Z_FIXED: i32 = 4;
    pub const Z_DEFAULT_STRATEGY: i32 = 0;
}

/// Errors that can occur during compression/decompression.
#[derive(Debug, Error)]
pub enum ZlibError {
    #[error("Compression error: {0}")]
    Compression(String),

    #[error("Decompression error: {0}")]
    Decompression(String),

    #[error("Invalid compression level: {0}")]
    InvalidLevel(i32),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Brotli error: {0}")]
    Brotli(String),
}

/// Compression options.
#[derive(Debug, Clone)]
pub struct CompressOptions {
    /// Compression level (0-9 for zlib, 0-11 for brotli).
    pub level: i32,
    /// Memory level (1-9).
    pub mem_level: u8,
    /// Compression strategy.
    pub strategy: i32,
    /// Window bits (8-15 for deflate, 9-15 for gzip).
    pub window_bits: i32,
}

impl Default for CompressOptions {
    fn default() -> Self {
        Self {
            level: constants::Z_DEFAULT_COMPRESSION,
            mem_level: 8,
            strategy: constants::Z_DEFAULT_STRATEGY,
            window_bits: 15,
        }
    }
}

/// Convert level to flate2 Compression.
fn to_flate2_level(level: i32) -> flate2::Compression {
    match level {
        constants::Z_NO_COMPRESSION => flate2::Compression::none(),
        constants::Z_BEST_SPEED => flate2::Compression::fast(),
        constants::Z_BEST_COMPRESSION => flate2::Compression::best(),
        constants::Z_DEFAULT_COMPRESSION => flate2::Compression::default(),
        l if (0..=9).contains(&l) => flate2::Compression::new(l as u32),
        _ => flate2::Compression::default(),
    }
}

// ============================================================================
// Gzip
// ============================================================================

/// Compress data using gzip.
pub fn gzip(data: &[u8], options: Option<CompressOptions>) -> Result<Vec<u8>, ZlibError> {
    let opts = options.unwrap_or_default();
    let level = to_flate2_level(opts.level);

    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), level);
    encoder.write_all(data)?;
    encoder
        .finish()
        .map_err(|e| ZlibError::Compression(e.to_string()))
}

/// Decompress gzip data.
pub fn gunzip(data: &[u8]) -> Result<Vec<u8>, ZlibError> {
    let mut decoder = flate2::read::GzDecoder::new(data);
    let mut result = Vec::new();
    decoder
        .read_to_end(&mut result)
        .map_err(|e| ZlibError::Decompression(e.to_string()))?;
    Ok(result)
}

// ============================================================================
// Deflate (raw)
// ============================================================================

/// Compress data using raw deflate.
pub fn deflate_raw(data: &[u8], options: Option<CompressOptions>) -> Result<Vec<u8>, ZlibError> {
    let opts = options.unwrap_or_default();
    let level = to_flate2_level(opts.level);

    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), level);
    encoder.write_all(data)?;
    encoder
        .finish()
        .map_err(|e| ZlibError::Compression(e.to_string()))
}

/// Decompress raw deflate data.
pub fn inflate_raw(data: &[u8]) -> Result<Vec<u8>, ZlibError> {
    let mut decoder = flate2::read::DeflateDecoder::new(data);
    let mut result = Vec::new();
    decoder
        .read_to_end(&mut result)
        .map_err(|e| ZlibError::Decompression(e.to_string()))?;
    Ok(result)
}

// ============================================================================
// Zlib (deflate with zlib header)
// ============================================================================

/// Compress data using zlib format (deflate with zlib header).
pub fn deflate(data: &[u8], options: Option<CompressOptions>) -> Result<Vec<u8>, ZlibError> {
    let opts = options.unwrap_or_default();
    let level = to_flate2_level(opts.level);

    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), level);
    encoder.write_all(data)?;
    encoder
        .finish()
        .map_err(|e| ZlibError::Compression(e.to_string()))
}

/// Decompress zlib format data.
pub fn inflate(data: &[u8]) -> Result<Vec<u8>, ZlibError> {
    let mut decoder = flate2::read::ZlibDecoder::new(data);
    let mut result = Vec::new();
    decoder
        .read_to_end(&mut result)
        .map_err(|e| ZlibError::Decompression(e.to_string()))?;
    Ok(result)
}

// ============================================================================
// Brotli
// ============================================================================

/// Compress data using brotli.
pub fn brotli_compress(data: &[u8], quality: Option<u32>) -> Result<Vec<u8>, ZlibError> {
    let quality = quality.unwrap_or(constants::BROTLI_DEFAULT_QUALITY);
    let quality = quality.min(constants::BROTLI_MAX_QUALITY);

    let mut output = Vec::new();
    {
        let params = brotli::enc::BrotliEncoderParams {
            quality: quality as i32,
            ..Default::default()
        };

        let mut writer = brotli::CompressorWriter::with_params(&mut output, 4096, &params);

        writer
            .write_all(data)
            .map_err(|e| ZlibError::Brotli(e.to_string()))?;
    }

    Ok(output)
}

/// Decompress brotli data.
pub fn brotli_decompress(data: &[u8]) -> Result<Vec<u8>, ZlibError> {
    let mut output = Vec::new();
    let mut reader = brotli::Decompressor::new(data, 4096);

    reader
        .read_to_end(&mut output)
        .map_err(|e| ZlibError::Brotli(e.to_string()))?;

    Ok(output)
}

// ============================================================================
// Utility functions
// ============================================================================

/// Calculate CRC32 checksum.
pub fn crc32(data: &[u8]) -> u32 {
    let mut hasher = flate2::Crc::new();
    hasher.update(data);
    hasher.sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_DATA: &[u8] = b"Hello, World! This is a test of the compression algorithms.";

    #[test]
    fn test_gzip_roundtrip() {
        let compressed = gzip(TEST_DATA, None).unwrap();
        let decompressed = gunzip(&compressed).unwrap();
        assert_eq!(decompressed, TEST_DATA);
    }

    #[test]
    fn test_gzip_levels() {
        for level in [
            constants::Z_NO_COMPRESSION,
            constants::Z_BEST_SPEED,
            constants::Z_BEST_COMPRESSION,
        ] {
            let opts = CompressOptions {
                level,
                ..Default::default()
            };
            let compressed = gzip(TEST_DATA, Some(opts)).unwrap();
            let decompressed = gunzip(&compressed).unwrap();
            assert_eq!(decompressed, TEST_DATA);
        }
    }

    #[test]
    fn test_deflate_roundtrip() {
        let compressed = deflate(TEST_DATA, None).unwrap();
        let decompressed = inflate(&compressed).unwrap();
        assert_eq!(decompressed, TEST_DATA);
    }

    #[test]
    fn test_deflate_raw_roundtrip() {
        let compressed = deflate_raw(TEST_DATA, None).unwrap();
        let decompressed = inflate_raw(&compressed).unwrap();
        assert_eq!(decompressed, TEST_DATA);
    }

    #[test]
    fn test_brotli_roundtrip() {
        let compressed = brotli_compress(TEST_DATA, None).unwrap();
        let decompressed = brotli_decompress(&compressed).unwrap();
        assert_eq!(decompressed, TEST_DATA);
    }

    #[test]
    fn test_brotli_quality_levels() {
        for quality in [0, 5, 11] {
            let compressed = brotli_compress(TEST_DATA, Some(quality)).unwrap();
            let decompressed = brotli_decompress(&compressed).unwrap();
            assert_eq!(decompressed, TEST_DATA);
        }
    }

    #[test]
    fn test_crc32() {
        let checksum = crc32(b"hello");
        assert_eq!(checksum, 0x3610a686);
    }

    #[test]
    fn test_empty_data() {
        let empty: &[u8] = b"";

        let gz = gzip(empty, None).unwrap();
        assert_eq!(gunzip(&gz).unwrap(), empty);

        let zlib = deflate(empty, None).unwrap();
        assert_eq!(inflate(&zlib).unwrap(), empty);

        let br = brotli_compress(empty, None).unwrap();
        assert_eq!(brotli_decompress(&br).unwrap(), empty);
    }

    #[test]
    fn test_large_data() {
        let large: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect();

        let compressed = gzip(&large, None).unwrap();
        let decompressed = gunzip(&compressed).unwrap();
        assert_eq!(decompressed, large);

        // Compression should reduce size significantly for repetitive data
        assert!(compressed.len() < large.len() / 2);
    }
}
