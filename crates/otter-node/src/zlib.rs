//! `node:zlib` native core — DEFLATE / zlib / gzip (de)compression + CRC-32.
//!
//! # Contents
//! - [`zlib_cjs_value`] builds the CommonJS `zlib` namespace (native core +
//!   `zlib.js` shim with `buffer` and `stream` deps).
//! - [`native_value`] exposes the raw codecs the shim drives: `deflateRaw`,
//!   `inflateRaw`, `deflate`, `inflate`, `gzip`, `gunzip`, `unzip`, `crc32`.
//!
//! # Invariants
//! - Bytes cross the native/JS boundary as **latin1 strings** (1 byte ↔ 1
//!   char; see [`bytes_to_latin1`]/[`latin1_to_bytes`]). This is the same
//!   one-shot bridge `fs`/`crypto` use — acceptable here because compression is
//!   a cold, bounded call, NOT a hot socket path.
//! - No capability is required: compression is pure computation, touching no fs
//!   / net / process surface.
//! - The backend is `flate2`'s default (`miniz_oxide`, pure Rust). Round-trips
//!   are byte-exact; a few Node tests that pin an exact compressed byte string
//!   (produced by zlib-ng) may differ — round-trip coverage is the bulk.
//!
//! # See also
//! - `zlib.js` — the JS surface (classes, async wrappers, constants, codes).
//! - `crypto.rs` — same latin1 bridge + `RuntimeObjectBuilder` dynamic-method
//!   pattern.

use std::io::{Read, Write};
use std::sync::Arc;

use flate2::Compression;
use flate2::read::{DeflateDecoder, GzDecoder, MultiGzDecoder, ZlibDecoder};
use flate2::write::{DeflateEncoder, GzEncoder, ZlibEncoder};
use otter_runtime::{
    CapabilitySet, RuntimeAttr, RuntimeNativeCtx as NativeCtx, RuntimeNativeError as NativeError,
    RuntimeObjectBuilder, RuntimeValue as Value, runtime_alloc_object, runtime_arg_to_string,
    runtime_native_dynamic,
};

const SHIM: &str = include_str!("zlib.js");

/// CommonJS export: the `zlib` namespace built by `zlib.js`.
pub fn zlib_cjs_value(ctx: &mut NativeCtx<'_>, caps: &CapabilitySet) -> Result<Value, String> {
    let native = native_value(ctx)?;
    let buffer = crate::buffer::buffer_cjs_value(ctx, caps)?;
    let stream = crate::stream::stream_cjs_value(ctx, caps)?;
    otter_runtime::run_builtin_cjs_shim(
        ctx,
        "node:zlib",
        SHIM,
        &[
            ("__zlibnative", native),
            ("buffer", buffer),
            ("stream", stream),
        ],
    )
}

/// ESM namespace install — CommonJS is the supported surface.
pub fn install_zlib_module(_ctx: &mut otter_runtime::HostedModuleCtx<'_>) -> Result<(), String> {
    Ok(())
}

fn native_value(ctx: &mut NativeCtx<'_>) -> Result<Value, String> {
    let object = runtime_alloc_object(ctx).map_err(|e| e.to_string())?;
    let mut builder = RuntimeObjectBuilder::from_object(ctx, object);

    macro_rules! m {
        ($name:literal, $len:expr, $f:ident) => {
            builder
                .method(
                    $name,
                    $len,
                    runtime_native_dynamic(Arc::new(
                        |ctx: &mut NativeCtx<'_>, args: &[Value], _c: &[Value]| $f(ctx, args),
                    )),
                    RuntimeAttr::builtin_function(),
                )
                .map_err(|e| e.to_string())?;
        };
    }

    m!("deflateRaw", 2, deflate_raw);
    m!("inflateRaw", 1, inflate_raw);
    m!("deflate", 2, deflate);
    m!("inflate", 1, inflate);
    m!("gzip", 2, gzip);
    m!("gunzip", 1, gunzip);
    m!("unzip", 1, unzip);
    m!("crc32", 2, crc32);

    Ok(Value::object(builder.build()))
}

fn bytes_to_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

fn latin1_to_bytes(s: &str) -> Vec<u8> {
    s.chars().map(|c| c as u32 as u8).collect()
}

/// Read `level` from args[index]; Node's `Z_DEFAULT_COMPRESSION` (-1) and any
/// out-of-range value fall back to flate2's default (level 6).
fn compression(args: &[Value], index: usize) -> Compression {
    match args.get(index).and_then(|v| v.as_f64()) {
        Some(n) if (0.0..=9.0).contains(&n) => Compression::new(n as u32),
        _ => Compression::default(),
    }
}

fn comp_err(op: &str, e: std::io::Error) -> NativeError {
    NativeError::Coded {
        kind: otter_vm::ErrorKind::Error,
        code: "Z_DATA_ERROR",
        message: format!("{op}: {e}"),
    }
}

fn deflate_raw(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let data = latin1_to_bytes(&runtime_arg_to_string(args, 0, ctx.heap()));
    let mut enc = DeflateEncoder::new(Vec::new(), compression(args, 1));
    enc.write_all(&data)
        .map_err(|e| comp_err("deflateRaw", e))?;
    let out = enc.finish().map_err(|e| comp_err("deflateRaw", e))?;
    crate::string_value(ctx, &bytes_to_latin1(&out))
}

fn inflate_raw(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let data = latin1_to_bytes(&runtime_arg_to_string(args, 0, ctx.heap()));
    let mut out = Vec::new();
    DeflateDecoder::new(&data[..])
        .read_to_end(&mut out)
        .map_err(|e| comp_err("inflateRaw", e))?;
    crate::string_value(ctx, &bytes_to_latin1(&out))
}

fn deflate(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let data = latin1_to_bytes(&runtime_arg_to_string(args, 0, ctx.heap()));
    let mut enc = ZlibEncoder::new(Vec::new(), compression(args, 1));
    enc.write_all(&data).map_err(|e| comp_err("deflate", e))?;
    let out = enc.finish().map_err(|e| comp_err("deflate", e))?;
    crate::string_value(ctx, &bytes_to_latin1(&out))
}

fn inflate(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let data = latin1_to_bytes(&runtime_arg_to_string(args, 0, ctx.heap()));
    let mut out = Vec::new();
    ZlibDecoder::new(&data[..])
        .read_to_end(&mut out)
        .map_err(|e| comp_err("inflate", e))?;
    crate::string_value(ctx, &bytes_to_latin1(&out))
}

fn gzip(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let data = latin1_to_bytes(&runtime_arg_to_string(args, 0, ctx.heap()));
    let mut enc = GzEncoder::new(Vec::new(), compression(args, 1));
    enc.write_all(&data).map_err(|e| comp_err("gzip", e))?;
    let out = enc.finish().map_err(|e| comp_err("gzip", e))?;
    crate::string_value(ctx, &bytes_to_latin1(&out))
}

fn gunzip(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let data = latin1_to_bytes(&runtime_arg_to_string(args, 0, ctx.heap()));
    // MultiGzDecoder consumes consecutive gzip members (Node concatenates them).
    let mut out = Vec::new();
    MultiGzDecoder::new(&data[..])
        .read_to_end(&mut out)
        .map_err(|e| comp_err("gunzip", e))?;
    crate::string_value(ctx, &bytes_to_latin1(&out))
}

/// `unzip` auto-detects the wrapper: gzip magic (0x1f 0x8b) → gunzip, else
/// treat as a zlib stream. Mirrors Node's `Unzip`.
fn unzip(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let data = latin1_to_bytes(&runtime_arg_to_string(args, 0, ctx.heap()));
    let mut out = Vec::new();
    if data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b {
        GzDecoder::new(&data[..])
            .read_to_end(&mut out)
            .map_err(|e| comp_err("unzip", e))?;
    } else {
        ZlibDecoder::new(&data[..])
            .read_to_end(&mut out)
            .map_err(|e| comp_err("unzip", e))?;
    }
    crate::string_value(ctx, &bytes_to_latin1(&out))
}

/// `crc32(dataLatin1, initialCrc)` → updated CRC-32 as a JS number.
fn crc32(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let data = latin1_to_bytes(&runtime_arg_to_string(args, 0, ctx.heap()));
    let init = args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0) as u32;
    Ok(Value::number(otter_vm::number::NumberValue::from_f64(
        crc32_with_seed(init, &data) as f64,
    )))
}

/// CRC-32 (IEEE) with an explicit starting value, matching zlib's `crc32`.
fn crc32_with_seed(seed: u32, data: &[u8]) -> u32 {
    let mut crc = !seed;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}
