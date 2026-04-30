//! `String(...)` constructor + `String.<static>` dispatcher.
//! Routed through [`crate::otter_bytecode::Op::StringCall`].
//!
//! # Contents
//! - [`call`] — single entry point used by the dispatch loop.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-string-constructor>
//! - <https://tc39.es/ecma262/#sec-string.fromcharcode>
//! - <https://tc39.es/ecma262/#sec-string.fromcodepoint>

use crate::number::NumberValue;
use crate::string::{JsString, StringHeap};
use crate::{Value, VmError};

/// Dispatch `String(...)` / `String.<name>(...)`. Empty `name`
/// (sentinel) selects the constructor.
///
/// # Errors
/// - [`VmError::TypeMismatch`] for malformed inputs.
/// - [`VmError::UnknownIntrinsic`] for unknown method names.
pub fn call(name: &str, args: &[Value], heap: &StringHeap) -> Result<Value, VmError> {
    match name {
        // §22.1.1 String(value) — coerce via §7.1.17 ToString.
        "" => {
            let s = match args.first() {
                Some(Value::String(s)) => s.to_lossy_string(),
                Some(Value::Symbol(_)) => {
                    // Spec: ToString(symbol) is a TypeError, but
                    // bare-call `String(symbol)` is allowed and
                    // returns the descriptive form.
                    args[0].display_string()
                }
                Some(other) => other.display_string(),
                None => String::new(),
            };
            Ok(Value::String(
                JsString::from_str(&s, heap).map_err(|_| VmError::TypeMismatch)?,
            ))
        }
        // §22.1.2.1 String.fromCharCode(...codeUnits).
        "fromCharCode" => {
            let mut units: Vec<u16> = Vec::with_capacity(args.len());
            for arg in args {
                let n = match arg {
                    Value::Number(n) => n.as_f64(),
                    _ => return Err(VmError::TypeMismatch),
                };
                let truncated = if n.is_finite() {
                    (n as i64) as u16
                } else {
                    0u16
                };
                units.push(truncated);
            }
            Ok(Value::String(
                JsString::from_utf16_units(&units, heap).map_err(|_| VmError::TypeMismatch)?,
            ))
        }
        // §22.1.2.2 String.fromCodePoint(...codePoints).
        "fromCodePoint" => {
            let mut units: Vec<u16> = Vec::with_capacity(args.len());
            for arg in args {
                let n = match arg {
                    Value::Number(n) => n.as_f64(),
                    _ => return Err(VmError::TypeMismatch),
                };
                if !n.is_finite() || n < 0.0 || n > 0x10FFFF as f64 || n.fract() != 0.0 {
                    return Err(VmError::TypeMismatch);
                }
                let cp = n as u32;
                if cp <= 0xFFFF {
                    units.push(cp as u16);
                } else {
                    // Surrogate pair encoding for supplementary
                    // planes.
                    let v = cp - 0x10000;
                    units.push(0xD800 | (v >> 10) as u16);
                    units.push(0xDC00 | (v & 0x3FF) as u16);
                }
            }
            Ok(Value::String(
                JsString::from_utf16_units(&units, heap).map_err(|_| VmError::TypeMismatch)?,
            ))
        }
        _ => Err(VmError::UnknownIntrinsic {
            name: format!("String.{name}"),
        }),
    }
}

/// `Number(value)` is wired alongside the global functions table
/// today — kept here for symmetry with the rest of the
/// constructor surface so a future `NumberCall` opcode has a
/// home. Currently unreferenced; the compiler still routes
/// `Number(x)` through `Op::ToNumber`.
#[allow(dead_code)]
pub fn _placeholder() -> NumberValue {
    NumberValue::from_i32(0)
}
