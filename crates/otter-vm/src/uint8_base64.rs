//! `Uint8Array` ↔ base64 / hex codecs (the `uint8array-base64`
//! proposal, ECMA-262 §23.2).
//!
//! Installs the `Uint8Array.fromBase64` / `fromHex` static methods and
//! the `Uint8Array.prototype.toBase64` / `toHex` /
//! `setFromBase64` / `setFromHex` methods. Only the `Uint8Array` kind
//! carries these — they are not shared on `%TypedArray%`.
//!
//! # Contents
//! - [`install_uint8_base64`] — post-bootstrap installer.
//! - codec helpers (`decode_base64`, `encode_base64`, hex variants).
//!
//! # See also
//! - <https://tc39.es/proposal-arraybuffer-base64/spec/>

use crate::binary::typed_array::{JsTypedArray, TypedArrayKind};
use crate::js_surface::JsSurfaceError;
use crate::object::{self, JsObject, PropertyDescriptor};
use crate::string::JsString;
use crate::{NativeCtx, NativeError, Value};

/// Final-chunk handling mode for base64 decoding (§ FromBase64).
#[derive(Clone, Copy, PartialEq, Eq)]
enum LastChunk {
    Loose,
    Strict,
    StopBeforePartial,
}

fn syntax(reason: &str) -> NativeError {
    NativeError::SyntaxError {
        name: "Uint8Array base64/hex",
        reason: reason.to_string(),
    }
}

fn type_err(reason: &str) -> NativeError {
    NativeError::TypeError {
        name: "Uint8Array base64/hex",
        reason: reason.to_string(),
    }
}

fn is_ascii_ws(u: u16) -> bool {
    matches!(u, 0x09 | 0x0A | 0x0C | 0x0D | 0x20)
}

/// Map a base64 code unit to its 6-bit value for the given alphabet.
fn b64_value(u: u16, url: bool) -> Option<u8> {
    let c = u8::try_from(u).ok()?;
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' if !url => Some(62),
        b'/' if !url => Some(63),
        b'-' if url => Some(62),
        b'_' if url => Some(63),
        _ => None,
    }
}

const B64_STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64_URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Whether the unused trailing bits of an under-full final chunk are
/// zero (required by `lastChunkHandling: "strict"`).
fn trailing_bits_zero(chunk: &[u8; 4], clen: usize) -> bool {
    match clen {
        2 => chunk[1] & 0x0F == 0, // 1 byte decoded; low 4 bits unused
        3 => chunk[2] & 0x03 == 0, // 2 bytes decoded; low 2 bits unused
        _ => true,
    }
}

fn decode_partial(chunk: &[u8; 4], clen: usize, out: &mut Vec<u8>) {
    if clen >= 2 {
        out.push((chunk[0] << 2) | (chunk[1] >> 4));
    }
    if clen >= 3 {
        out.push((chunk[1] << 4) | (chunk[2] >> 2));
    }
}

/// § FromBase64 — decode `units` into bytes, stopping once `max_bytes`
/// output bytes have been produced. Returns the decoded bytes, the
/// number of code units consumed (`read`), and an optional error.
/// Bytes decoded before the error are still returned (so `setFromBase64`
/// can write them before throwing — "writes up to error").
fn decode_base64(
    units: &[u16],
    url: bool,
    lch: LastChunk,
    max_bytes: usize,
) -> (Vec<u8>, usize, Option<NativeError>) {
    // § FromBase64 step 3 — a zero output budget reads nothing.
    if max_bytes == 0 {
        return (Vec::new(), 0, None);
    }
    let mut bytes: Vec<u8> = Vec::new();
    let mut read = 0usize;
    let mut chunk = [0u8; 4];
    let mut clen = 0usize;
    let mut i = 0usize;
    let n = units.len();
    loop {
        while i < n && is_ascii_ws(units[i]) {
            i += 1;
        }
        if i == n {
            if clen > 0 {
                match lch {
                    LastChunk::StopBeforePartial => return (bytes, read, None),
                    LastChunk::Loose => {
                        if clen == 1 {
                            return (bytes, read, Some(syntax("single trailing character")));
                        }
                        if bytes.len() + (clen - 1) > max_bytes {
                            return (bytes, read, None);
                        }
                        decode_partial(&chunk, clen, &mut bytes);
                        read = n;
                    }
                    LastChunk::Strict => {
                        return (bytes, read, Some(syntax("missing padding")));
                    }
                }
            } else {
                read = n;
            }
            return (bytes, read, None);
        }
        let c = units[i];
        if c == b'=' as u16 {
            if clen < 2 {
                return (bytes, read, Some(syntax("unexpected padding")));
            }
            i += 1;
            if clen == 2 {
                // A two-character chunk needs a second `=` to complete.
                while i < n && is_ascii_ws(units[i]) {
                    i += 1;
                }
                if i == n || units[i] != b'=' as u16 {
                    // Incomplete padding: stop-before-partial stops at
                    // the start of this partial chunk; the others throw.
                    if lch == LastChunk::StopBeforePartial {
                        return (bytes, read, None);
                    }
                    return (bytes, read, Some(syntax("malformed padding")));
                }
                i += 1;
            }
            while i < n && is_ascii_ws(units[i]) {
                i += 1;
            }
            if i != n {
                return (bytes, read, Some(syntax("trailing data after padding")));
            }
            if lch == LastChunk::Strict && !trailing_bits_zero(&chunk, clen) {
                return (bytes, read, Some(syntax("non-zero padding bits")));
            }
            if bytes.len() + (clen - 1) > max_bytes {
                return (bytes, read, None);
            }
            decode_partial(&chunk, clen, &mut bytes);
            read = n;
            return (bytes, read, None);
        }
        let Some(v) = b64_value(c, url) else {
            return (bytes, read, Some(syntax("invalid base64 character")));
        };
        chunk[clen] = v;
        clen += 1;
        i += 1;
        if clen == 4 {
            if bytes.len() + 3 > max_bytes {
                // No room for a full output chunk — stop before it.
                return (bytes, read, None);
            }
            bytes.push((chunk[0] << 2) | (chunk[1] >> 4));
            bytes.push((chunk[1] << 4) | (chunk[2] >> 2));
            bytes.push((chunk[2] << 6) | chunk[3]);
            clen = 0;
            read = i;
            // Output budget exhausted — stop before any trailing data
            // (which is not validated once the buffer is full).
            if bytes.len() >= max_bytes {
                return (bytes, read, None);
            }
        }
    }
}

/// § encode-to-base64.
fn encode_base64(bytes: &[u8], url: bool, omit_padding: bool) -> String {
    let table = if url { B64_URL } else { B64_STD };
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut chunks = bytes.chunks_exact(3);
    for c in &mut chunks {
        let n = (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]);
        out.push(table[(n >> 18 & 0x3F) as usize] as char);
        out.push(table[(n >> 12 & 0x3F) as usize] as char);
        out.push(table[(n >> 6 & 0x3F) as usize] as char);
        out.push(table[(n & 0x3F) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        1 => {
            let n = u32::from(rem[0]) << 16;
            out.push(table[(n >> 18 & 0x3F) as usize] as char);
            out.push(table[(n >> 12 & 0x3F) as usize] as char);
            if !omit_padding {
                out.push_str("==");
            }
        }
        2 => {
            let n = (u32::from(rem[0]) << 16) | (u32::from(rem[1]) << 8);
            out.push(table[(n >> 18 & 0x3F) as usize] as char);
            out.push(table[(n >> 12 & 0x3F) as usize] as char);
            out.push(table[(n >> 6 & 0x3F) as usize] as char);
            if !omit_padding {
                out.push('=');
            }
        }
        _ => {}
    }
    out
}

/// § FromHex — decode hex `units` into bytes, up to `max_bytes` output
/// bytes. Returns the bytes and the number of code units consumed.
fn decode_hex(units: &[u16], max_bytes: usize) -> (Vec<u8>, usize, Option<NativeError>) {
    // § FromHex — an odd-length input is a SyntaxError before any byte
    // is produced (nothing is written by setFromHex in that case).
    if !units.len().is_multiple_of(2) {
        return (
            Vec::new(),
            0,
            Some(syntax("hex string length must be even")),
        );
    }
    let mut bytes = Vec::new();
    let mut i = 0usize;
    while i + 2 <= units.len() {
        if bytes.len() >= max_bytes {
            return (bytes, i, None);
        }
        match (hex_value(units[i]), hex_value(units[i + 1])) {
            (Some(h), Some(l)) => bytes.push((h << 4) | l),
            _ => return (bytes, i, Some(syntax("invalid hex character"))),
        }
        i += 2;
    }
    (bytes, i, None)
}

fn hex_value(u: u16) -> Option<u8> {
    let c = u8::try_from(u).ok()?;
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
    out
}

// ---------------------------------------------------------------
// Option reading
// ---------------------------------------------------------------

/// § GetOption — read a string-valued option observably (firing a
/// getter), `ToString`-coerce it, and validate against `allowed`.
/// `undefined` / absent yields the default (first `allowed` entry).
fn read_string_option(
    ctx: &mut NativeCtx<'_>,
    options: Option<&Value>,
    key: &str,
    allowed: &[&'static str],
) -> Result<&'static str, NativeError> {
    let default = allowed[0];
    let Some(opts) = options.copied().filter(|v| !v.is_undefined()) else {
        return Ok(default);
    };
    let value = crate::regexp_prototype::get_property_runtime(ctx, &opts, key, "Uint8Array")?;
    if value.is_undefined() {
        return Ok(default);
    }
    // The option must be a primitive String — no ToString coercion (a
    // String wrapper object or a `toString`-bearing object throws).
    if !value.is_string() {
        return Err(type_err("option value must be a string"));
    }
    let s = value
        .as_string(ctx.heap())
        .map(|s| s.to_lossy_string(ctx.heap()))
        .ok_or_else(|| type_err("option value must be a string"))?;
    allowed
        .iter()
        .copied()
        .find(|a| *a == s)
        .ok_or_else(|| type_err("invalid option value"))
}

/// § GetOption (boolean) — read observably and `ToBoolean`-coerce.
fn read_bool_option(
    ctx: &mut NativeCtx<'_>,
    options: Option<&Value>,
    key: &str,
) -> Result<bool, NativeError> {
    let Some(opts) = options.copied().filter(|v| !v.is_undefined()) else {
        return Ok(false);
    };
    let value = crate::regexp_prototype::get_property_runtime(ctx, &opts, key, "Uint8Array")?;
    Ok(value.to_boolean(ctx.heap()))
}

fn options_must_be_object(ctx: &NativeCtx<'_>, options: Option<&Value>) -> Result<(), NativeError> {
    if let Some(v) = options
        && !v.is_undefined()
        && v.as_object().is_none()
    {
        return Err(type_err("options is not an object"));
    }
    let _ = ctx;
    Ok(())
}

fn last_chunk_from_str(s: &str) -> LastChunk {
    match s {
        "strict" => LastChunk::Strict,
        "stop-before-partial" => LastChunk::StopBeforePartial,
        _ => LastChunk::Loose,
    }
}

// ---------------------------------------------------------------
// Result construction
// ---------------------------------------------------------------

fn string_units<'a>(ctx: &NativeCtx<'a>, arg: Option<&Value>) -> Result<Vec<u16>, NativeError> {
    let v = arg.copied().unwrap_or_else(Value::undefined);
    if !v.is_string() {
        return Err(type_err("argument must be a string"));
    }
    let s = v
        .as_string(ctx.heap())
        .ok_or_else(|| type_err("argument must be a string"))?;
    Ok(s.to_utf16_vec(ctx.heap()))
}

fn fresh_uint8array(ctx: &mut NativeCtx<'_>, bytes: &[u8]) -> Result<Value, NativeError> {
    let buf = ctx
        .alloc_array_buffer_zeroed(bytes.len(), &[], &[])
        .map_err(|_| type_err("out of memory"))?
        .ok_or_else(|| type_err("allocation failed"))?;
    let view = JsTypedArray::new(ctx.heap_mut(), buf, TypedArrayKind::Uint8, 0, bytes.len())
        .map_err(|_| type_err("out of memory"))?;
    for (i, &b) in bytes.iter().enumerate() {
        view.set(ctx.heap_mut(), i, &Value::number_i32(b as i32));
    }
    Ok(Value::typed_array(view))
}

// ---------------------------------------------------------------
// Native methods
// ---------------------------------------------------------------

fn u8_from_base64(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let units = string_units(ctx, args.first())?;
    options_must_be_object(ctx, args.get(1))?;
    let url =
        read_string_option(ctx, args.get(1), "alphabet", &["base64", "base64url"])? == "base64url";
    let lch = last_chunk_from_str(read_string_option(
        ctx,
        args.get(1),
        "lastChunkHandling",
        &["loose", "strict", "stop-before-partial"],
    )?);
    let (bytes, _read, err) = decode_base64(&units, url, lch, usize::MAX);
    if let Some(e) = err {
        return Err(e);
    }
    fresh_uint8array(ctx, &bytes)
}

fn u8_from_hex(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let units = string_units(ctx, args.first())?;
    let (bytes, _read, err) = decode_hex(&units, usize::MAX);
    if let Some(e) = err {
        return Err(e);
    }
    fresh_uint8array(ctx, &bytes)
}

fn receiver_uint8(ctx: &NativeCtx<'_>, name: &'static str) -> Result<JsTypedArray, NativeError> {
    let t = ctx
        .this_value()
        .as_typed_array(ctx.heap())
        .ok_or_else(|| NativeError::TypeError {
            name,
            reason: "receiver is not a Uint8Array".to_string(),
        })?;
    if t.kind() != TypedArrayKind::Uint8 {
        return Err(NativeError::TypeError {
            name,
            reason: "receiver is not a Uint8Array".to_string(),
        });
    }
    Ok(t)
}

fn receiver_bytes(ctx: &mut NativeCtx<'_>, t: &JsTypedArray) -> Result<Vec<u8>, NativeError> {
    if t.is_out_of_bounds(ctx.heap()) {
        return Err(type_err("Uint8Array is detached or out of bounds"));
    }
    let len = t.length(ctx.heap_mut());
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let v = t
            .get(ctx.heap_mut(), i)
            .map_err(|_| type_err("read failed"))?;
        out.push(v.as_number().map_or(0, |n| n.as_f64() as i64 as u8));
    }
    Ok(out)
}

fn u8_to_base64(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver_uint8(ctx, "Uint8Array.prototype.toBase64")?;
    options_must_be_object(ctx, args.first())?;
    let url =
        read_string_option(ctx, args.first(), "alphabet", &["base64", "base64url"])? == "base64url";
    let omit_padding = read_bool_option(ctx, args.first(), "omitPadding")?;
    let bytes = receiver_bytes(ctx, &t)?;
    let s = encode_base64(&bytes, url, omit_padding);
    let js = JsString::from_str(&s, ctx.heap_mut()).map_err(|_| type_err("out of memory"))?;
    Ok(Value::string(js))
}

fn u8_to_hex(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver_uint8(ctx, "Uint8Array.prototype.toHex")?;
    let bytes = receiver_bytes(ctx, &t)?;
    let s = encode_hex(&bytes);
    let js = JsString::from_str(&s, ctx.heap_mut()).map_err(|_| type_err("out of memory"))?;
    Ok(Value::string(js))
}

/// Build the `{ read, written }` result Record returned by the
/// `setFrom*` methods (ordinary object, enumerable data properties).
fn set_result(ctx: &mut NativeCtx<'_>, read: usize, written: usize) -> Result<Value, NativeError> {
    let obj = ctx
        .alloc_object_with_roots(&[], &[])
        .map_err(|_| type_err("out of memory"))?;
    object::define_own_property(
        obj,
        ctx.heap_mut(),
        "read",
        PropertyDescriptor::data(Value::number_i32(read as i32), true, true, true),
    );
    object::define_own_property(
        obj,
        ctx.heap_mut(),
        "written",
        PropertyDescriptor::data(Value::number_i32(written as i32), true, true, true),
    );
    Ok(Value::object(obj))
}

/// Write decoded `bytes` into `t[0..]` (the caller has range-limited the
/// decode to `t.length`).
fn write_into(ctx: &mut NativeCtx<'_>, t: &JsTypedArray, bytes: &[u8]) {
    for (k, &b) in bytes.iter().enumerate() {
        t.set(ctx.heap_mut(), k, &Value::number_i32(b as i32));
    }
}

fn u8_set_from_base64(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver_uint8(ctx, "Uint8Array.prototype.setFromBase64")?;
    let units = string_units(ctx, args.first())?;
    options_must_be_object(ctx, args.get(1))?;
    let url =
        read_string_option(ctx, args.get(1), "alphabet", &["base64", "base64url"])? == "base64url";
    let lch = last_chunk_from_str(read_string_option(
        ctx,
        args.get(1),
        "lastChunkHandling",
        &["loose", "strict", "stop-before-partial"],
    )?);
    if t.is_out_of_bounds(ctx.heap()) {
        return Err(type_err("Uint8Array is detached or out of bounds"));
    }
    let max = t.length(ctx.heap_mut());
    let (bytes, read, err) = decode_base64(&units, url, lch, max);
    // Write the bytes decoded so far, then surface any error.
    write_into(ctx, &t, &bytes);
    if let Some(e) = err {
        return Err(e);
    }
    set_result(ctx, read, bytes.len())
}

fn u8_set_from_hex(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver_uint8(ctx, "Uint8Array.prototype.setFromHex")?;
    let units = string_units(ctx, args.first())?;
    if t.is_out_of_bounds(ctx.heap()) {
        return Err(type_err("Uint8Array is detached or out of bounds"));
    }
    let max = t.length(ctx.heap_mut());
    let (bytes, read, err) = decode_hex(&units, max);
    write_into(ctx, &t, &bytes);
    if let Some(e) = err {
        return Err(e);
    }
    set_result(ctx, read, bytes.len())
}

// ---------------------------------------------------------------
// Installer
// ---------------------------------------------------------------

/// Install the base64 / hex codec methods on `Uint8Array` and its
/// prototype. Runs in the post-bootstrap phase once the constructor
/// exists.
pub fn install_uint8_base64(
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    let Some(ctor) = object::get(global, heap, "Uint8Array").and_then(|v| v.as_native_function())
    else {
        return Ok(());
    };
    let ctor_value = Value::native_function(ctor);
    let proto = match ctor
        .own_property_descriptor(heap, "prototype")
        .ok()
        .flatten()
        .and_then(|d| match d.kind {
            crate::object::DescriptorKind::Data { value } => value.as_object(),
            _ => None,
        }) {
        Some(p) => p,
        None => return Ok(()),
    };
    let proto_value = Value::object(proto);

    fn make_fn(
        heap: &mut otter_gc::GcHeap,
        name: &'static str,
        length: u8,
        call: crate::native_function::NativeFastFn,
        root: &Value,
    ) -> Result<Value, JsSurfaceError> {
        let f = crate::bootstrap::native_static_with_value_roots(heap, name, length, call, &[root])
            .map_err(|_| JsSurfaceError::OutOfMemory)?;
        Ok(Value::native_function(f))
    }

    // Statics on the constructor.
    let from_base64 = make_fn(heap, "fromBase64", 1, u8_from_base64, &ctor_value)?;
    ctor.define_own_property(
        heap,
        "fromBase64",
        PropertyDescriptor::data(from_base64, true, false, true),
    );
    let from_hex = make_fn(heap, "fromHex", 1, u8_from_hex, &ctor_value)?;
    ctor.define_own_property(
        heap,
        "fromHex",
        PropertyDescriptor::data(from_hex, true, false, true),
    );
    // Methods on Uint8Array.prototype.
    let to_base64 = make_fn(heap, "toBase64", 0, u8_to_base64, &proto_value)?;
    object::define_own_property(
        proto,
        heap,
        "toBase64",
        PropertyDescriptor::data(to_base64, true, false, true),
    );
    let to_hex = make_fn(heap, "toHex", 0, u8_to_hex, &proto_value)?;
    object::define_own_property(
        proto,
        heap,
        "toHex",
        PropertyDescriptor::data(to_hex, true, false, true),
    );
    let set_from_base64 = make_fn(heap, "setFromBase64", 1, u8_set_from_base64, &proto_value)?;
    object::define_own_property(
        proto,
        heap,
        "setFromBase64",
        PropertyDescriptor::data(set_from_base64, true, false, true),
    );
    let set_from_hex = make_fn(heap, "setFromHex", 1, u8_set_from_hex, &proto_value)?;
    object::define_own_property(
        proto,
        heap,
        "setFromHex",
        PropertyDescriptor::data(set_from_hex, true, false, true),
    );
    Ok(())
}
