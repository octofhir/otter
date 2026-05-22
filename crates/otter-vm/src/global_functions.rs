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
                None | Some(Value::Undefined) => 0i32,
                Some(Value::Number(n)) => match n.as_smi() {
                    Some(v) => v,
                    None => n.as_f64() as i32,
                },
                _ => 0,
            };
            Ok(Value::Number(number::parse_int(&s, radix)))
        }
        // §19.2.4 / §21.1.2.12 — same callable for both names.
        M::ParseFloat => Ok(Value::Number(number::parse_float(&coerce_to_string(
            args.first(),
            gc_heap,
        )))),
        // §19.2.3 — coerces, then defers to the strict predicate.
        M::IsNaN => {
            let value = args.first().cloned().unwrap_or(Value::Undefined);
            Ok(Value::Boolean(number::is_nan(number::to_number_value(
                &value, gc_heap,
            ))))
        }
        M::IsFinite => {
            let value = args.first().cloned().unwrap_or(Value::Undefined);
            Ok(Value::Boolean(number::is_finite(number::to_number_value(
                &value, gc_heap,
            ))))
        }
        // §21.1.2.3 / §21.1.2.2 — strict, no coercion.
        M::NumberIsNaN => {
            let value = args.first().cloned().unwrap_or(Value::Undefined);
            Ok(Value::Boolean(matches!(
                value,
                Value::Number(ref n) if number::is_nan(n.as_f64())
            )))
        }
        M::NumberIsFinite => {
            let value = args.first().cloned().unwrap_or(Value::Undefined);
            Ok(Value::Boolean(matches!(
                value,
                Value::Number(ref n) if number::is_finite(n.as_f64())
            )))
        }
        M::NumberIsInteger => Ok(Value::Boolean(number::is_integer(
            &args.first().cloned().unwrap_or(Value::Undefined),
        ))),
        M::NumberIsSafeInteger => Ok(Value::Boolean(number::is_safe_integer(
            &args.first().cloned().unwrap_or(Value::Undefined),
        ))),
        M::EncodeURI => js_string(
            &uri_encode(&coerce_to_string(args.first(), gc_heap), false),
            gc_heap,
        ),
        M::EncodeURIComponent => js_string(
            &uri_encode(&coerce_to_string(args.first(), gc_heap), true),
            gc_heap,
        ),
        M::DecodeURI => {
            let out = uri_decode(&coerce_to_string(args.first(), gc_heap), false)?;
            js_string(&out, gc_heap)
        }
        M::DecodeURIComponent => {
            let out = uri_decode(&coerce_to_string(args.first(), gc_heap), true)?;
            js_string(&out, gc_heap)
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
            Ok(Value::String(
                JsString::from_utf16_units(&decoded, gc_heap).map_err(|_| VmError::TypeMismatch)?,
            ))
        }
    }
}

/// Coerce an argument to a UTF-16 buffer. Mirrors `coerce_to_string`
/// but preserves the spec's "string of code units" view that
/// `escape` / `unescape` walk.
fn coerce_to_utf16(arg: Option<&Value>, heap: &otter_gc::GcHeap) -> Vec<u16> {
    match arg {
        None | Some(Value::Undefined) => "undefined".encode_utf16().collect(),
        Some(Value::String(s)) => s.to_utf16_vec(heap),
        Some(other) => other.display_string(heap).encode_utf16().collect(),
    }
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
        None | Some(Value::Undefined) => "undefined".to_string(),
        Some(other) => other.display_string(heap),
    }
}

fn js_string(s: &str, heap: &mut otter_gc::GcHeap) -> Result<Value, VmError> {
    Ok(Value::String(
        JsString::from_str(s, heap).map_err(|_| VmError::TypeMismatch)?,
    ))
}

/// §19.2.6.5 Encode — percent-encode every byte that isn't in the
/// "always-safe" set. `component = true` matches encodeURIComponent
/// (smaller safe set); `false` matches encodeURI.
fn uri_encode(input: &str, component: bool) -> String {
    fn is_unreserved(b: u8) -> bool {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
    }
    fn is_uri_reserved(b: u8) -> bool {
        matches!(
            b,
            b';' | b'/' | b'?' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b',' | b'#'
        )
    }
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        if is_unreserved(b) || (!component && is_uri_reserved(b)) {
            out.push(b as char);
        } else {
            out.push('%');
            const HEX: &[u8] = b"0123456789ABCDEF";
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0F) as usize] as char);
        }
    }
    out
}

/// §19.2.6.4 Decode — inverse of [`uri_encode`]. Raises
/// `TypeMismatch` (eventually `URIError`) on malformed escapes.
fn uri_decode(input: &str, _component: bool) -> Result<String, VmError> {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            if i + 2 >= bytes.len() {
                return Err(VmError::TypeMismatch);
            }
            let hi = hex_digit(bytes[i + 1])?;
            let lo = hex_digit(bytes[i + 2])?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| VmError::TypeMismatch)
}

fn hex_digit(b: u8) -> Result<u8, VmError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(VmError::TypeMismatch),
    }
}
