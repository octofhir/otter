//! Node.js zlib module implementation.
//!
//! Provides compression/decompression using gzip, deflate, and brotli algorithms.
//! Uses flate2 (with zlib-rs backend) for gzip/deflate and brotli crate for brotli.

use std::io::{Read, Write};
use thiserror::Error;

use flate2::write::{DeflateEncoder, ZlibEncoder};
use flate2::{Compress, Compression, Decompress, FlushDecompress, Status};

pub(crate) const DEFAULT_CHUNK_SIZE: usize = 16 * 1024;

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
    /// Chunk size for streaming inflations.
    pub chunk_size: usize,
    /// Optional compression dictionary.
    pub dictionary: Option<Vec<u8>>,
}

impl Default for CompressOptions {
    fn default() -> Self {
        Self {
            level: constants::Z_DEFAULT_COMPRESSION,
            mem_level: 8,
            strategy: constants::Z_DEFAULT_STRATEGY,
            window_bits: 15,
            chunk_size: DEFAULT_CHUNK_SIZE,
            dictionary: None,
        }
    }
}

fn normalize_window_bits(bits: i32) -> u8 {
    let bits = bits.abs();
    bits.clamp(9, 15) as u8
}

fn compression_error_message(err: &flate2::CompressError) -> String {
    format!("{}", err)
}

fn decompression_error_message(err: &flate2::DecompressError) -> String {
    err.to_string()
}

fn apply_compression_dictionary(
    compression: &mut Compress,
    dictionary: &[u8],
) -> Result<(), ZlibError> {
    compression
        .set_dictionary(dictionary)
        .map(|_| ())
        .map_err(|err| ZlibError::Compression(compression_error_message(&err)))
}

fn apply_decompression_dictionary(
    decompression: &mut Decompress,
    dictionary: &[u8],
) -> Result<(), ZlibError> {
    decompression
        .set_dictionary(dictionary)
        .map(|_| ())
        .map_err(|err| ZlibError::Decompression(decompression_error_message(&err)))
}

fn compress_with_options(
    data: &[u8],
    options: Option<CompressOptions>,
    header: bool,
) -> Result<Vec<u8>, ZlibError> {
    let opts = options.unwrap_or_default();
    let level = to_flate2_level(opts.level);
    let window_bits = normalize_window_bits(opts.window_bits);
    let mut compressor = Compress::new_with_window_bits(level, header, window_bits);

    if let Some(dictionary) = opts.dictionary.as_ref() {
        apply_compression_dictionary(&mut compressor, dictionary)?;
    }

    // Use raw Compress API for full control over window bits and dictionary
    let mut output = Vec::with_capacity(data.len());
    let mut input_offset = 0;

    loop {
        let input = &data[input_offset..];
        let before_out = compressor.total_out();
        let before_in = compressor.total_in();

        output.reserve(output.len() + 4096);
        let output_len = output.len();
        output.resize(output.capacity(), 0);

        let flush = if input.is_empty() {
            flate2::FlushCompress::Finish
        } else {
            flate2::FlushCompress::None
        };

        let status = compressor
            .compress(input, &mut output[output_len..], flush)
            .map_err(|e| ZlibError::Compression(compression_error_message(&e)))?;

        let consumed = (compressor.total_in() - before_in) as usize;
        let produced = (compressor.total_out() - before_out) as usize;

        input_offset += consumed;
        output.truncate(output_len + produced);

        match status {
            flate2::Status::StreamEnd => break,
            flate2::Status::Ok | flate2::Status::BufError => {
                if input.is_empty() && produced == 0 {
                    break;
                }
            }
        }
    }

    Ok(output)
}

fn decompress_with_options(
    data: &[u8],
    options: Option<CompressOptions>,
    header: bool,
) -> Result<Vec<u8>, ZlibError> {
    let opts = options.unwrap_or_default();
    let window_bits = normalize_window_bits(opts.window_bits);
    let chunk_size = opts.chunk_size.max(1);

    let mut decompressor = Decompress::new_with_window_bits(header, window_bits);
    if let Some(dictionary) = opts.dictionary.as_ref() {
        apply_decompression_dictionary(&mut decompressor, dictionary)?;
    }

    let mut output = Vec::new();
    let mut input_offset = 0usize;
    let mut total_out = 0usize;

    loop {
        let mut buffer = vec![0u8; chunk_size];
        let status = decompressor
            .decompress(&data[input_offset..], &mut buffer, FlushDecompress::Finish)
            .map_err(|err| ZlibError::Decompression(decompression_error_message(&err)))?;

        let consumed = (decompressor.total_in() as usize).saturating_sub(input_offset);
        let produced = (decompressor.total_out() as usize).saturating_sub(total_out);

        input_offset = input_offset.saturating_add(consumed);
        total_out = total_out.saturating_add(produced);
        output.extend_from_slice(&buffer[..produced]);

        if status == Status::StreamEnd {
            break;
        }

        if consumed == 0 && produced == 0 {
            return Err(ZlibError::Decompression(
                "decompression did not make progress".to_string(),
            ));
        }

        if consumed == 0 {
            break;
        }
    }

    Ok(output)
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
    compress_with_options(data, options, false)
}

/// Decompress raw deflate data.
pub fn inflate_raw(data: &[u8], options: Option<CompressOptions>) -> Result<Vec<u8>, ZlibError> {
    decompress_with_options(data, options, false)
}

// ============================================================================
// Zlib (deflate with zlib header)
// ============================================================================

/// Compress data using zlib format (deflate with zlib header).
pub fn deflate(data: &[u8], options: Option<CompressOptions>) -> Result<Vec<u8>, ZlibError> {
    compress_with_options(data, options, true)
}

/// Decompress zlib format data.
pub fn inflate(data: &[u8], options: Option<CompressOptions>) -> Result<Vec<u8>, ZlibError> {
    decompress_with_options(data, options, true)
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
        let decompressed = inflate(&compressed, None).unwrap();
        assert_eq!(decompressed, TEST_DATA);
    }

    #[test]
    fn test_deflate_raw_roundtrip() {
        let compressed = deflate_raw(TEST_DATA, None).unwrap();
        let decompressed = inflate_raw(&compressed, None).unwrap();
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
        assert_eq!(inflate(&zlib, None).unwrap(), empty);

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
