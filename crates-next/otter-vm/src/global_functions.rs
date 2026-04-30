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
use crate::string::{JsString, StringHeap};
use crate::{Value, VmError};

/// Dispatch `<name>(args...)`. `name` is one of the §19.2 global
/// names or a `Number.<x>` static prefixed with `Number.`.
///
/// # Errors
/// - [`VmError::UnknownIntrinsic`] when `name` isn't recognised.
/// - [`VmError::TypeMismatch`] for malformed inputs to `decodeURI*`.
pub fn call(name: &str, args: &[Value], heap: &StringHeap) -> Result<Value, VmError> {
    match name {
        // `parseInt` and `Number.parseInt` are the same callable
        // per §21.1.2.13. Foundation routes both names here.
        "parseInt" | "Number.parseInt" => {
            let s = coerce_to_string(args.first());
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
        // §21.1.2.12 — same callable for both names.
        "parseFloat" | "Number.parseFloat" => Ok(Value::Number(number::parse_float(
            &coerce_to_string(args.first()),
        ))),
        // §19.2.3 — coerces, then defers to the strict predicate.
        "isNaN" => {
            let value = args.first().cloned().unwrap_or(Value::Undefined);
            Ok(Value::Boolean(number::is_nan(number::to_number_value(
                &value,
            ))))
        }
        "isFinite" => {
            let value = args.first().cloned().unwrap_or(Value::Undefined);
            Ok(Value::Boolean(number::is_finite(number::to_number_value(
                &value,
            ))))
        }
        // §21.1.2.3 / §21.1.2.2 — strict, no coercion.
        "Number.isNaN" => {
            let value = args.first().cloned().unwrap_or(Value::Undefined);
            Ok(Value::Boolean(matches!(
                value,
                Value::Number(ref n) if number::is_nan(n.as_f64())
            )))
        }
        "Number.isFinite" => {
            let value = args.first().cloned().unwrap_or(Value::Undefined);
            Ok(Value::Boolean(matches!(
                value,
                Value::Number(ref n) if number::is_finite(n.as_f64())
            )))
        }
        "Number.isInteger" => Ok(Value::Boolean(number::is_integer(
            &args.first().cloned().unwrap_or(Value::Undefined),
        ))),
        "Number.isSafeInteger" => Ok(Value::Boolean(number::is_safe_integer(
            &args.first().cloned().unwrap_or(Value::Undefined),
        ))),
        "encodeURI" => js_string(&uri_encode(&coerce_to_string(args.first()), false), heap),
        "encodeURIComponent" => js_string(&uri_encode(&coerce_to_string(args.first()), true), heap),
        "decodeURI" => {
            let out = uri_decode(&coerce_to_string(args.first()), false)?;
            js_string(&out, heap)
        }
        "decodeURIComponent" => {
            let out = uri_decode(&coerce_to_string(args.first()), true)?;
            js_string(&out, heap)
        }
        _ => Err(VmError::UnknownIntrinsic {
            name: format!("global {name}"),
        }),
    }
}

fn coerce_to_string(arg: Option<&Value>) -> String {
    match arg {
        None | Some(Value::Undefined) => "undefined".to_string(),
        Some(other) => other.display_string(),
    }
}

fn js_string(s: &str, heap: &StringHeap) -> Result<Value, VmError> {
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
