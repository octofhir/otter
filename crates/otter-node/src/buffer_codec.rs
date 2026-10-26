//! Text and a buffer's bytes, in both directions.
//!
//! # Contents
//! - [`codec_native_cjs_value`] — the `__bufcodec` module the `buffer` shim
//!   reaches for: one slice (bytes to text) entry point per encoding, and
//!   `encode`, `write` and `byteLength` (text to bytes) taking the encoding
//!   by name.
//!
//! # Invariants
//! - A decode reads the view's bytes once and builds one string; an encode
//!   reads the string's code units once, at their stored width, and writes
//!   the bytes once. Doing either a character at a time in JavaScript costs
//!   an allocation per byte.
//! - Malformed input is replaced, never refused, as Node's codecs do: a lone
//!   surrogate encodes as U+FFFD in UTF-8, hex stops at the first pair it
//!   cannot read, base64 skips characters outside its alphabet and stops at
//!   padding.
//! - `write` stores only whole characters: a UTF-8 sequence or UTF-16 unit
//!   the space ends inside is left out.
//!
//! # See also
//! - `crates/otter-node/src/buffer.js` — the shim that calls this.

use otter_runtime::ValueWindow;
use otter_runtime::{CapabilitySet, RuntimeNativeError as NativeError, RuntimeTaskSpawner};
use otter_vm::string::CodeUnits;
use otter_vm::{Local, NativeCtx, NativeScope, Value};

/// CommonJS export: the codec entry points.
///
/// # Errors
/// Returns a native error when a member cannot be allocated or defined.
pub fn codec_native_cjs_value<'scope>(
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
            move |ctx: &mut NativeCtx<'_>, args: ValueWindow<'_>, _captures: ValueWindow<'_>| {
                slice(ctx, args, encoding)
            },
        )?;
        scope.set(object, name, method)?;
    }
    let encode = scope.native_closure(
        "encode",
        2,
        &[],
        |ctx: &mut NativeCtx<'_>, args: ValueWindow<'_>, _captures: ValueWindow<'_>| {
            let bytes = encoded(ctx, &args);
            ctx.scope(|mut scope| {
                let buffer = scope.array_buffer_from_bytes(bytes)?;
                Ok(scope.finish(buffer))
            })
        },
    )?;
    scope.set(object, "encode", encode)?;
    let write = scope.native_closure(
        "write",
        5,
        &[],
        |ctx: &mut NativeCtx<'_>, args: ValueWindow<'_>, _captures: ValueWindow<'_>| {
            write_into(ctx, &args)
        },
    )?;
    scope.set(object, "write", write)?;
    let byte_length = scope.native_closure(
        "byteLength",
        2,
        &[],
        |ctx: &mut NativeCtx<'_>, args: ValueWindow<'_>, _captures: ValueWindow<'_>| {
            let encoding = encoding_arg(ctx, args.get(1).copied());
            let length = args
                .first()
                .and_then(|value| value.as_string(ctx.heap()))
                .map_or(0, |text| {
                    text.with_code_units(ctx.heap(), |units| byte_length(units, encoding))
                });
            Ok(Value::number(otter_vm::NumberValue::from_f64(
                length as f64,
            )))
        },
    )?;
    scope.set(object, "byteLength", byte_length)?;
    Ok(object)
}

/// How a run of bytes reads as text.
#[derive(Clone, Copy, PartialEq, Eq)]
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
    args: ValueWindow<'_>,
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
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
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

/// The encoding a shim call names; the shim passes only normalized names.
fn encoding_arg(ctx: &NativeCtx<'_>, value: Option<Value>) -> Encoding {
    let name = value
        .and_then(|value| value.as_string(ctx.heap()))
        .map(|name| name.to_lossy_string(ctx.heap()));
    match name.as_deref() {
        Some("latin1") => Encoding::Latin1,
        Some("ascii") => Encoding::Ascii,
        Some("utf16le") => Encoding::Utf16le,
        Some("hex") => Encoding::Hex,
        Some("base64") => Encoding::Base64,
        Some("base64url") => Encoding::Base64Url,
        _ => Encoding::Utf8,
    }
}

/// `encode(string, encoding)`'s bytes.
fn encoded(ctx: &NativeCtx<'_>, args: &ValueWindow<'_>) -> Vec<u8> {
    let encoding = encoding_arg(ctx, args.get(1).copied());
    args.first()
        .and_then(|value| value.as_string(ctx.heap()))
        .map_or_else(Vec::new, |text| {
            text.with_code_units(ctx.heap(), |units| encode(units, encoding))
        })
}

/// `write(view, string, offset, length, encoding)`: store the string's whole
/// characters that fit in `length` bytes at `offset`; the bytes stored.
fn write_into(ctx: &mut NativeCtx<'_>, args: &ValueWindow<'_>) -> Result<Value, NativeError> {
    let number = |index: usize| {
        args.get(index)
            .and_then(|value| value.as_f64())
            .unwrap_or(0.0)
            .max(0.0) as usize
    };
    let encoding = encoding_arg(ctx, args.get(4).copied());
    let mut bytes = args
        .get(1)
        .and_then(|value| value.as_string(ctx.heap()))
        .map_or_else(Vec::new, |text| {
            text.with_code_units(ctx.heap(), |units| encode(units, encoding))
        });
    let Some(view) = args
        .first()
        .and_then(|value| value.as_typed_array(ctx.heap()))
    else {
        return Ok(Value::number(otter_vm::NumberValue::from_f64(0.0)));
    };
    let heap = ctx.heap();
    let base = view.byte_offset(heap);
    let capacity = view.byte_length(heap);
    let offset = number(2).min(capacity);
    let room = number(3).min(capacity - offset);
    let count = whole_characters(&bytes, room, encoding);
    bytes.truncate(count);
    let buffer = view.buffer(heap);
    buffer.with_bytes_mut(ctx.heap_mut(), |target| {
        target[base + offset..base + offset + count].copy_from_slice(&bytes);
    });
    Ok(Value::number(otter_vm::NumberValue::from_f64(count as f64)))
}

/// How many of `bytes` fit in `room` without splitting a character.
fn whole_characters(bytes: &[u8], room: usize, encoding: Encoding) -> usize {
    let mut count = bytes.len().min(room);
    match encoding {
        Encoding::Utf8 if count < bytes.len() => {
            // Back up over the continuation bytes of the sequence cut short.
            while count > 0 && bytes[count] & 0xc0 == 0x80 {
                count -= 1;
            }
        }
        Encoding::Utf16le => count &= !1,
        _ => {}
    }
    count
}

/// A string's code units as bytes in `encoding`.
fn encode(units: CodeUnits<'_>, encoding: Encoding) -> Vec<u8> {
    match (encoding, units) {
        (Encoding::Utf8, CodeUnits::Latin1(bytes)) => {
            let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 4);
            for &byte in bytes {
                if byte < 0x80 {
                    out.push(byte);
                } else {
                    out.extend_from_slice(&[0xc0 | (byte >> 6), 0x80 | (byte & 0x3f)]);
                }
            }
            out
        }
        (Encoding::Utf8, CodeUnits::Utf16(units)) => {
            let mut out = Vec::with_capacity(units.len() * 3 / 2);
            let mut buffer = [0; 4];
            for unit in char::decode_utf16(units.iter().copied()) {
                let ch = unit.unwrap_or(char::REPLACEMENT_CHARACTER);
                out.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
            }
            out
        }
        (Encoding::Latin1 | Encoding::Ascii, CodeUnits::Latin1(bytes)) => bytes.to_vec(),
        (Encoding::Latin1 | Encoding::Ascii, CodeUnits::Utf16(units)) => {
            units.iter().map(|unit| *unit as u8).collect()
        }
        (Encoding::Utf16le, CodeUnits::Latin1(bytes)) => {
            bytes.iter().flat_map(|&byte| [byte, 0]).collect()
        }
        (Encoding::Utf16le, CodeUnits::Utf16(units)) => {
            units.iter().flat_map(|unit| unit.to_le_bytes()).collect()
        }
        (Encoding::Hex, units) => hex_bytes(&widened(units)),
        (Encoding::Base64 | Encoding::Base64Url, units) => base64_bytes(&widened(units)),
    }
}

fn widened(units: CodeUnits<'_>) -> Vec<u16> {
    match units {
        CodeUnits::Latin1(bytes) => bytes.iter().map(|&byte| u16::from(byte)).collect(),
        CodeUnits::Utf16(units) => units.to_vec(),
    }
}

/// The byte length a string encodes to, without encoding it.
fn byte_length(units: CodeUnits<'_>, encoding: Encoding) -> usize {
    match (encoding, units) {
        (Encoding::Utf8, CodeUnits::Latin1(bytes)) => {
            bytes.len() + bytes.iter().filter(|&&byte| byte >= 0x80).count()
        }
        (Encoding::Utf8, CodeUnits::Utf16(units)) => char::decode_utf16(units.iter().copied())
            .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER).len_utf8())
            .sum(),
        (Encoding::Latin1 | Encoding::Ascii, units) => match units {
            CodeUnits::Latin1(bytes) => bytes.len(),
            CodeUnits::Utf16(units) => units.len(),
        },
        (Encoding::Utf16le, units) => {
            2 * match units {
                CodeUnits::Latin1(bytes) => bytes.len(),
                CodeUnits::Utf16(units) => units.len(),
            }
        }
        (Encoding::Hex, units) => {
            (match units {
                CodeUnits::Latin1(bytes) => bytes.len(),
                CodeUnits::Utf16(units) => units.len(),
            }) / 2
        }
        (Encoding::Base64 | Encoding::Base64Url, units) => {
            // Node's estimate from the length: up to two trailing `=` do not
            // count, every four characters make three bytes.
            let units = widened(units);
            let mut length = units.len();
            for _ in 0..2 {
                if length > 0 && units[length - 1] == u16::from(b'=') {
                    length -= 1;
                }
            }
            (length * 3) >> 2
        }
    }
}

fn hex_bytes(units: &[u16]) -> Vec<u8> {
    let nibble = |unit: u16| char::from_u32(u32::from(unit))?.to_digit(16);
    let mut out = Vec::with_capacity(units.len() / 2);
    for pair in units.as_chunks::<2>().0 {
        // Node's hex decoder stops at the first byte it cannot fully read.
        let (Some(high), Some(low)) = (nibble(pair[0]), nibble(pair[1])) else {
            break;
        };
        out.push(((high << 4) | low) as u8);
    }
    out
}

/// A base64 or base64url symbol's value.
fn base64_value(unit: u16) -> Option<u8> {
    let byte = u8::try_from(unit).ok()?;
    Some(match byte {
        b'A'..=b'Z' => byte - b'A',
        b'a'..=b'z' => byte - b'a' + 26,
        b'0'..=b'9' => byte - b'0' + 52,
        b'+' | b'-' => 62,
        b'/' | b'_' => 63,
        _ => return None,
    })
}

fn base64_bytes(units: &[u16]) -> Vec<u8> {
    let symbols: Vec<u8> = units
        .iter()
        .take_while(|&&unit| unit != u16::from(b'='))
        .filter_map(|&unit| base64_value(unit))
        .collect();
    let mut out = Vec::with_capacity(symbols.len() * 3 / 4);
    for group in symbols.chunks(4) {
        let packed = group
            .iter()
            .enumerate()
            .fold(0_u32, |packed, (index, &value)| {
                packed | u32::from(value) << (18 - 6 * index)
            });
        // Two symbols carry one byte, three carry two, four carry three; a
        // single trailing symbol carries none.
        let carried = group.len().saturating_sub(1);
        out.extend_from_slice(&packed.to_be_bytes()[1..1 + carried]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_encode_as_node_encodes_them() {
        let latin = CodeUnits::Latin1(b"h\xe9");
        assert_eq!(encode(latin, Encoding::Utf8), "hé".as_bytes());
        let lone: Vec<u16> = vec![0x61, 0xd800, 0x62];
        assert_eq!(
            encode(CodeUnits::Utf16(&lone), Encoding::Utf8),
            "a\u{fffd}b".as_bytes()
        );
        assert_eq!(
            encode(CodeUnits::Utf16(&lone), Encoding::Latin1),
            [0x61, 0x00, 0x62]
        );
        assert_eq!(
            encode(CodeUnits::Latin1(b"0aFfzz"), Encoding::Hex),
            [0x0a, 0xff]
        );
        assert_eq!(
            encode(CodeUnits::Latin1(b"aGVs bG8=xx"), Encoding::Base64),
            b"hello"
        );
        assert_eq!(
            encode(CodeUnits::Latin1(b"_-8"), Encoding::Base64Url),
            [0xff, 0xef]
        );
        assert_eq!(byte_length(CodeUnits::Utf16(&lone), Encoding::Utf8), 5);
        assert_eq!(
            byte_length(CodeUnits::Latin1(b"aGVsbG8="), Encoding::Base64),
            5
        );
        assert_eq!(
            byte_length(CodeUnits::Latin1(b"aGVs bG8=xx"), Encoding::Base64),
            8
        );
    }

    #[test]
    fn a_write_stores_only_whole_characters() {
        let bytes = "aé€".as_bytes();
        assert_eq!(whole_characters(bytes, 2, Encoding::Utf8), 1);
        assert_eq!(whole_characters(bytes, 3, Encoding::Utf8), 3);
        assert_eq!(whole_characters(bytes, 5, Encoding::Utf8), 3);
        assert_eq!(whole_characters(bytes, 6, Encoding::Utf8), 6);
        assert_eq!(whole_characters(&[1, 2, 3], 3, Encoding::Utf16le), 2);
    }
}
