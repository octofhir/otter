//! Turning a buffer's bytes into text.
//!
//! # Contents
//! - [`decode_native_cjs_value`] — the `__bufdecode` module the `buffer` shim
//!   reaches for, with one entry point per encoding it can be asked for.
//!
//! # Invariants
//! - A decode reads the view's bytes once and builds one string. Doing it a
//!   character at a time in JavaScript costs a rope node per byte, which a
//!   megabyte of output turns into an exhausted heap.
//! - Malformed input is replaced, never refused: `Buffer#toString` has no
//!   fatal mode, and every encoding here answers with text.
//!
//! # See also
//! - `crates/otter-node/src/buffer.js` — the shim that calls this.

use otter_runtime::{CapabilitySet, RuntimeNativeError as NativeError, RuntimeTaskSpawner};
use otter_vm::{Local, NativeCtx, NativeScope, Value};

/// CommonJS export: the byte-to-text entry points.
///
/// # Errors
/// Returns a native error when a member cannot be allocated or defined.
pub fn decode_native_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    _module: Local<'scope>,
    _require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    let object = scope.object()?;
    for (name, encoding) in [
        ("utf8Slice", Encoding::Utf8),
        ("latin1Slice", Encoding::Latin1),
        ("asciiSlice", Encoding::Ascii),
        ("utf16leSlice", Encoding::Utf16le),
        ("hexSlice", Encoding::Hex),
        ("base64Slice", Encoding::Base64),
        ("base64urlSlice", Encoding::Base64Url),
    ] {
        let method = scope.native_closure(
            name,
            3,
            &[],
            move |ctx: &mut NativeCtx<'_>, args: &[Value], _captures: &[Value]| {
                slice(ctx, args, encoding)
            },
        )?;
        scope.set(object, name, method)?;
    }
    Ok(object)
}

/// How a run of bytes reads as text.
#[derive(Clone, Copy)]
enum Encoding {
    Utf8,
    Latin1,
    Ascii,
    Utf16le,
    Hex,
    Base64,
    Base64Url,
}

fn slice(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    encoding: Encoding,
) -> Result<Value, NativeError> {
    let offset = |index: usize| {
        args.get(index)
            .and_then(|value| value.as_f64())
            .unwrap_or(0.0)
            .max(0.0) as usize
    };
    let start = offset(1);
    let end = offset(2);
    let bytes = args
        .first()
        .copied()
        .and_then(|value| view_bytes(ctx, value, start, end))
        .unwrap_or_default();
    let text = match encoding {
        Encoding::Utf8 => String::from_utf8_lossy(&bytes).into_owned(),
        Encoding::Latin1 => bytes.iter().map(|byte| char::from(*byte)).collect(),
        Encoding::Ascii => bytes.iter().map(|byte| char::from(*byte & 0x7f)).collect(),
        Encoding::Utf16le => utf16le(&bytes),
        Encoding::Hex => hex(&bytes),
        Encoding::Base64 => base64(&bytes, false),
        Encoding::Base64Url => base64(&bytes, true),
    };
    ctx.scope(|mut scope| {
        let text = scope.string(&text)?;
        Ok(scope.finish(text))
    })
}

/// The bytes a typed array covers between two offsets, clamped to what it has.
fn view_bytes(ctx: &mut NativeCtx<'_>, value: Value, start: usize, end: usize) -> Option<Vec<u8>> {
    let view = value.as_typed_array(ctx.heap())?;
    let heap = ctx.heap();
    let base = view.byte_offset(heap);
    let length = view.byte_length(heap);
    let start = start.min(length);
    let end = end.min(length).max(start);
    Some(
        view.buffer(heap)
            .with_bytes(heap, |bytes| bytes[base + start..base + end].to_vec()),
    )
}

fn utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

fn base64(bytes: &[u8], url: bool) -> String {
    let alphabet: &[u8; 64] = if url {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
    } else {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
    };
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let packed = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        out.push(char::from(alphabet[(packed >> 18) as usize & 0x3f]));
        out.push(char::from(alphabet[(packed >> 12) as usize & 0x3f]));
        if chunk.len() > 1 {
            out.push(char::from(alphabet[(packed >> 6) as usize & 0x3f]));
        } else if !url {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(char::from(alphabet[packed as usize & 0x3f]));
        } else if !url {
            out.push('=');
        }
    }
    out
}
