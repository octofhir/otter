//! §19.2 global-function dispatcher.
//!
//! **Pure aliases.** `parseInt` / `parseFloat` / `isNaN` /
//! `isFinite` are defined by ECMA-262 as the same callables as
//! `Number.parseInt` / `Number.parseFloat` / `Number.isNaN` /
//! `Number.isFinite` (§19.2.5 → §21.1.2.13, etc.). Every numeric
//! algorithm lives in [`crate::number::parse`]; this module is the
//! dispatcher the compiler routes both shapes through.
//!
//! Spec-faithful coercion difference:
//! - The global `isNaN(x)` / `isFinite(x)` first coerce `x` via
//!   §7.1.4 ToNumber, then call the strict predicate.
//! - `Number.isNaN(x)` / `Number.isFinite(x)` skip coercion.
//!
//! Both forms reach the **same** strict predicate
//! ([`crate::number::is_nan`] / [`crate::number::is_finite`]) — the
//! coercion step is the only delta, sitting at the dispatcher
//! level.
//!
//! # Contents
//! - [`call`] — single entry point used by the dispatch loop.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-function-properties-of-the-global-object>
//! - <https://tc39.es/ecma262/#sec-properties-of-the-number-constructor>

use crate::number;
use crate::string::JsString;
use crate::{Value, VmError};

/// Dispatch `<method>(args...)`. Routes the typed
/// [`GlobalMethod`] emitted by the compiler — covers both the
/// §19.2 globals and the `Number.<predicate>` aliases.
///
/// # Errors
/// - [`VmError::TypeMismatch`] for malformed inputs to `decodeURI*`.
pub fn call(
    method: otter_bytecode::method_id::GlobalMethod,
    args: &[Value],
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<Value, VmError> {
    use otter_bytecode::method_id::GlobalMethod as M;
    match method {
        // §19.2.5 / §21.1.2.13 — `parseInt` and `Number.parseInt`
        // are the same callable. Compiler emits `ParseInt` for both.
        M::ParseInt => {
            let s = coerce_to_string(args.first(), gc_heap);
            let radix = match args.get(1) {
                Some(v) if let Some(n) = v.as_number() => {
                    n.as_smi().unwrap_or_else(|| n.as_f64() as i32)
                }
                Some(v) if v.is_undefined() => 0,
                None => 0,
                _ => 0,
            };
            Ok(Value::number(number::parse_int(&s, radix)))
        }
        // §19.2.4 / §21.1.2.12 — same callable for both names.
        M::ParseFloat => Ok(Value::number(number::parse_float(&coerce_to_string(
            args.first(),
            gc_heap,
        )))),
        // §19.2.3 — coerces, then defers to the strict predicate.
        M::IsNaN => {
            let value = args.first().cloned().unwrap_or(Value::undefined());
            Ok(Value::boolean(number::is_nan(number::to_number_value(
                &value, gc_heap,
            ))))
        }
        M::IsFinite => {
            let value = args.first().cloned().unwrap_or(Value::undefined());
            Ok(Value::boolean(number::is_finite(number::to_number_value(
                &value, gc_heap,
            ))))
        }
        // §21.1.2.3 / §21.1.2.2 — strict, no coercion.
        M::NumberIsNaN => {
            let value = args.first().cloned().unwrap_or(Value::undefined());
            Ok(Value::boolean(
                value
                    .as_number()
                    .is_some_and(|n| number::is_nan(n.as_f64())),
            ))
        }
        M::NumberIsFinite => {
            let value = args.first().cloned().unwrap_or(Value::undefined());
            Ok(Value::boolean(
                value
                    .as_number()
                    .is_some_and(|n| number::is_finite(n.as_f64())),
            ))
        }
        M::NumberIsInteger => Ok(Value::boolean(number::is_integer(
            &args.first().cloned().unwrap_or(Value::undefined()),
        ))),
        M::NumberIsSafeInteger => Ok(Value::boolean(number::is_safe_integer(
            &args.first().cloned().unwrap_or(Value::undefined()),
        ))),
        M::EncodeURI => js_string(
            &uri_encode(&coerce_to_utf16(args.first(), gc_heap), false)?,
            gc_heap,
        ),
        M::EncodeURIComponent => js_string(
            &uri_encode(&coerce_to_utf16(args.first(), gc_heap), true)?,
            gc_heap,
        ),
        M::DecodeURI => {
            let out = uri_decode(&coerce_to_utf16(args.first(), gc_heap), false)?;
            Ok(Value::string(
                JsString::from_utf16_units(&out, gc_heap).map_err(|_| VmError::TypeMismatch)?,
            ))
        }
        M::DecodeURIComponent => {
            let out = uri_decode(&coerce_to_utf16(args.first(), gc_heap), true)?;
            Ok(Value::string(
                JsString::from_utf16_units(&out, gc_heap).map_err(|_| VmError::TypeMismatch)?,
            ))
        }
        // §B.2.1.1 `escape(string)` — legacy AnnexB encoder. Walks
        // the UTF-16 code units; preserves the spec's "static
        // unencoded" set, emits `%XX` for code points below 256,
        // and `%uXXXX` for the rest.
        M::Escape => js_string(
            &legacy_escape(&coerce_to_utf16(args.first(), gc_heap)),
            gc_heap,
        ),
        // §B.2.1.2 `unescape(string)` — legacy AnnexB decoder.
        // Recognises `%XX` and `%uXXXX` sequences, copies other
        // code units unchanged.
        M::Unescape => {
            let units = coerce_to_utf16(args.first(), gc_heap);
            let decoded = legacy_unescape(&units);
            Ok(Value::string(
                JsString::from_utf16_units(&decoded, gc_heap).map_err(|_| VmError::TypeMismatch)?,
            ))
        }
    }
}

/// Coerce an argument to a UTF-16 buffer. Mirrors `coerce_to_string`
/// but preserves the spec's "string of code units" view that
/// `escape` / `unescape` walk.
fn coerce_to_utf16(arg: Option<&Value>, heap: &otter_gc::GcHeap) -> Vec<u16> {
    let Some(value) = arg else {
        return "undefined".encode_utf16().collect();
    };
    if value.is_undefined() {
        return "undefined".encode_utf16().collect();
    }
    if let Some(s) = value.as_string(heap) {
        return s.to_utf16_vec(heap);
    }
    value.display_string(heap).encode_utf16().collect()
}

/// §B.2.1.1 `escape(string)` — emit ASCII alphanumerics plus the
/// static "unencoded" set verbatim. Anything else turns into
/// `%XX` (single-byte code points) or `%uXXXX` (everything else).
fn legacy_escape(units: &[u16]) -> String {
    const HEX: &[u8] = b"0123456789ABCDEF";
    fn is_unescaped(c: u16) -> bool {
        if c < 128 {
            let b = c as u8;
            b.is_ascii_alphanumeric() || matches!(b, b'@' | b'*' | b'_' | b'+' | b'-' | b'.' | b'/')
        } else {
            false
        }
    }
    let mut out = String::with_capacity(units.len());
    for &c in units {
        if is_unescaped(c) {
            out.push(c as u8 as char);
        } else if c < 256 {
            out.push('%');
            out.push(HEX[(c >> 4) as usize] as char);
            out.push(HEX[(c & 0x0F) as usize] as char);
        } else {
            out.push('%');
            out.push('u');
            out.push(HEX[((c >> 12) & 0x0F) as usize] as char);
            out.push(HEX[((c >> 8) & 0x0F) as usize] as char);
            out.push(HEX[((c >> 4) & 0x0F) as usize] as char);
            out.push(HEX[(c & 0x0F) as usize] as char);
        }
    }
    out
}

/// §B.2.1.2 `unescape(string)` — invert `legacy_escape`. Walks
/// the code-unit buffer once; copies any code unit that isn't part
/// of a well-formed `%XX` or `%uXXXX` escape verbatim. Malformed
/// escapes are preserved literally (spec quirk — `%G` stays `%G`).
fn legacy_unescape(units: &[u16]) -> Vec<u16> {
    fn hex_u16(c: u16) -> Option<u16> {
        match c as u32 {
            v @ (0x30..=0x39) => Some(v as u16 - 0x30),
            v @ (0x41..=0x46) => Some(v as u16 - 0x41 + 10),
            v @ (0x61..=0x66) => Some(v as u16 - 0x61 + 10),
            _ => None,
        }
    }
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        if units[i] == b'%' as u16 {
            if i + 5 < units.len()
                && units[i + 1] == b'u' as u16
                && let (Some(a), Some(b), Some(c), Some(d)) = (
                    hex_u16(units[i + 2]),
                    hex_u16(units[i + 3]),
                    hex_u16(units[i + 4]),
                    hex_u16(units[i + 5]),
                )
            {
                out.push((a << 12) | (b << 8) | (c << 4) | d);
                i += 6;
                continue;
            }
            if i + 2 < units.len()
                && let (Some(hi), Some(lo)) = (hex_u16(units[i + 1]), hex_u16(units[i + 2]))
            {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(units[i]);
        i += 1;
    }
    out
}

fn coerce_to_string(arg: Option<&Value>, heap: &otter_gc::GcHeap) -> String {
    match arg {
        None => "undefined".to_string(),
        Some(v) if v.is_undefined() => "undefined".to_string(),
        Some(other) => other.display_string(heap),
    }
}

fn js_string(s: &str, heap: &mut otter_gc::GcHeap) -> Result<Value, VmError> {
    Ok(Value::string(
        JsString::from_str(s, heap).map_err(|_| VmError::TypeMismatch)?,
    ))
}

/// §19.2.6.5 Encode — percent-encode every byte that isn't in the
/// "always-safe" set. `component = true` matches encodeURIComponent
/// (smaller safe set); `false` matches encodeURI.
/// §19.2.6.5 Encode — walks the input's UTF-16 code units. A code unit
/// in the unescaped set passes through; any other code point is UTF-8
/// encoded and percent-escaped byte by byte. An unpaired surrogate is
/// malformed (`URIError`).
fn uri_encode(units: &[u16], component: bool) -> Result<String, VmError> {
    fn is_unreserved(b: u16) -> bool {
        b < 0x80
            && (u8::try_from(b).unwrap().is_ascii_alphanumeric()
                || matches!(
                    b as u8,
                    b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
                ))
    }
    fn is_uri_reserved(b: u16) -> bool {
        b < 0x80
            && matches!(
                b as u8,
                b';' | b'/' | b'?' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b',' | b'#'
            )
    }
    const HEX: &[u8] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let c = units[i];
        if is_unreserved(c) || (!component && is_uri_reserved(c)) {
            out.push(c as u8 as char);
            i += 1;
            continue;
        }
        // §11.1.3 CodePointAt — pair a high surrogate with the following
        // low surrogate; an unpaired surrogate (either half) is malformed.
        let code_point: u32 = if (0xD800..=0xDBFF).contains(&c) {
            match units.get(i + 1) {
                Some(&low) if (0xDC00..=0xDFFF).contains(&low) => {
                    i += 1;
                    0x10000 + (((c as u32 - 0xD800) << 10) | (low as u32 - 0xDC00))
                }
                _ => return Err(uri_error()),
            }
        } else if (0xDC00..=0xDFFF).contains(&c) {
            return Err(uri_error());
        } else {
            c as u32
        };
        i += 1;
        // Encode the scalar value as UTF-8 and percent-escape each byte.
        let mut buf = [0u8; 4];
        let encoded = char::from_u32(code_point)
            .ok_or_else(uri_error)?
            .encode_utf8(&mut buf);
        for b in encoded.bytes() {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0F) as usize] as char);
        }
    }
    Ok(out)
}

/// §19.2.6.7 Decode — inverse of [`uri_encode`]. Walks the input's
/// UTF-16 code units (so a lone surrogate that isn't part of a `%XX`
/// escape passes through unchanged) and emits UTF-16. Each `%XX`
/// escape contributes one octet; the octet stream is decoded as
/// well-formed UTF-8, rejecting overlong forms, encoded surrogates,
/// and truncated sequences with a `URIError`. When `component` is
/// false (`decodeURI`), a decoded octet whose code point is in the
/// reserved set `;/?:@&=+$,#` keeps its original `%XX` spelling
/// instead of being decoded.
fn uri_decode(units: &[u16], component: bool) -> Result<Vec<u16>, VmError> {
    fn is_reserved(b: u8) -> bool {
        matches!(
            b,
            b';' | b'/' | b'?' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b',' | b'#'
        )
    }
    // Read the two hex digits following a `%` at `units[at]`, returning
    // the decoded octet. Malformed / truncated escapes are URIErrors.
    fn escaped_octet(units: &[u16], at: usize) -> Result<u8, VmError> {
        let hi = units.get(at + 1).copied().ok_or_else(uri_error)?;
        let lo = units.get(at + 2).copied().ok_or_else(uri_error)?;
        let hi = u8::try_from(hi).ok().ok_or_else(uri_error)?;
        let lo = u8::try_from(lo).ok().ok_or_else(uri_error)?;
        Ok((hex_digit(hi)? << 4) | hex_digit(lo)?)
    }

    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    let len = units.len();
    let mut k = 0;
    while k < len {
        let c = units[k];
        if c != b'%' as u16 {
            out.push(c);
            k += 1;
            continue;
        }
        let start = k;
        let b = escaped_octet(units, k)?;
        k += 3;
        if b & 0x80 == 0 {
            // Single octet. decodeURI keeps reserved characters encoded.
            if !component && is_reserved(b) {
                out.extend_from_slice(&units[start..start + 3]);
            } else {
                out.push(b as u16);
            }
            continue;
        }
        // Multi-octet UTF-8: the leading byte's high-one run gives the
        // sequence length (2..=4); 0b10xxxxxx or >4 is malformed.
        let n = match b {
            0b1100_0000..=0b1101_1111 => 2,
            0b1110_0000..=0b1110_1111 => 3,
            0b1111_0000..=0b1111_0111 => 4,
            _ => return Err(uri_error()),
        };
        let mut cp: u32 = (b & (0x7F >> n)) as u32;
        for _ in 1..n {
            if units.get(k).copied() != Some(b'%' as u16) {
                return Err(uri_error());
            }
            let cont = escaped_octet(units, k)?;
            if cont & 0xC0 != 0x80 {
                return Err(uri_error());
            }
            cp = (cp << 6) | (cont & 0x3F) as u32;
            k += 3;
        }
        // Reject overlong encodings, out-of-range, and encoded surrogates.
        let min = [0, 0, 0x80, 0x800, 0x1_0000][n];
        if cp < min || cp > 0x10_FFFF || (0xD800..=0xDFFF).contains(&cp) {
            return Err(uri_error());
        }
        if cp <= 0xFFFF {
            out.push(cp as u16);
        } else {
            let v = cp - 0x1_0000;
            out.push((0xD800 + (v >> 10)) as u16);
            out.push((0xDC00 + (v & 0x3FF)) as u16);
        }
    }
    Ok(out)
}

fn uri_error() -> VmError {
    VmError::URIError
}

fn hex_digit(b: u8) -> Result<u8, VmError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(uri_error()),
    }
}
